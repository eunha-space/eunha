//! Moderation list subscriptions: domain lists fetched on a schedule, whose
//! entries are applied as domain blocks or allows, or suggested to moderators.
//! All of it behind `experimental_features = ["moderation_subscriptions"]`.
//!
//! Mastodon: app/models/moderation_subscription.rb
//! Mastodon: app/models/moderation_subscription_advisory.rb
//! Mastodon: app/models/moderation_suggestion.rb
//! Mastodon: app/services/moderation_subscription_sync_service.rb
//! Mastodon: app/services/process_moderation_lists_service.rb
//! Mastodon: app/services/apply_moderation_suggestion_service.rb
//! Mastodon: app/workers/scheduler/moderation_list_synchronization_scheduler.rb

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use chrono::NaiveDateTime;
use serde::Deserialize;

use crate::{
    api::mastodon::admin::{spawn_block, stricter_than, unallow_domain},
    db::models::domain_severity,
    federation::tag_manager::normalize_domain,
    moderation::action_log::{log_for_subscription, Target},
    state::AppState,
};

pub const FEATURE: &str = "moderation_subscriptions";

/// `Account.representative`, which `admin_action_logs` shows as automation.
pub const REPRESENTATIVE_ID: i64 = crate::federation::instance_actor::INSTANCE_ACTOR_ID;

/// `ModerationSubscription.types`.
pub const TYPE_CSV_LIST: i32 = 0;
pub const TYPE_JSON: i32 = 1;

/// `ModerationSuggestion.actions`; an advisory has all but `RETRACT`, and a
/// subscription's `list_action` only `ACCEPT` and `REJECT`.
pub const RETRACT: i32 = -1;
pub const ACCEPT: i32 = 0;
pub const REJECT: i32 = 1;
pub const LIMIT: i32 = 2;

/// `ModerationSuggestion.states`.
pub const STATE_NEW: i32 = 0;
pub const STATE_MAILED: i32 = 1;
pub const STATE_APPLIED: i32 = 2;
pub const STATE_DISMISSED: i32 = 3;

/// `target_type: :domain`, the only type there is.
pub const TARGET_DOMAIN: i32 = 0;

/// `Admin::Import::ROWS_PROCESSING_LIMIT`.
const ROWS_PROCESSING_LIMIT: usize = 20_000;

/// `Request::Response#body_with_limit`'s default, one mebibyte.
const BODY_LIMIT: usize = 1024 * 1024;

pub fn action_name(action: i32) -> &'static str {
    match action {
        RETRACT => "retract",
        ACCEPT => "accept",
        REJECT => "reject",
        _ => "limit",
    }
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: i64,
    pub name: Option<String>,
    pub url: String,
    pub priority: Option<i32>,
    pub kind: i32,
    pub list_action: Option<i32>,
    pub apply_automatically: bool,
    pub preserve_relationships: bool,
    pub retract_automatically: bool,
    pub override_unattributed: bool,
    pub last_synced_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
}

impl Subscription {
    /// `#apply_conditions`.
    pub fn apply_conditions(&self) -> &'static str {
        match (self.apply_automatically, self.preserve_relationships) {
            (false, _) => "never",
            (true, true) => "safely",
            (true, false) => "always",
        }
    }
}

/// `ModerationSubscription.order(priority: :asc)`.
pub async fn all(state: &AppState) -> Result<Vec<Subscription>> {
    Ok(sqlx::query_as!(
        Subscription,
        r#"SELECT id, name, url, priority, type AS kind, list_action, apply_automatically,
                  preserve_relationships, retract_automatically, override_unattributed,
                  last_synced_at, created_at
           FROM moderation_subscriptions ORDER BY priority ASC NULLS LAST, id ASC"#
    )
    .fetch_all(&state.db)
    .await?)
}

pub async fn find(state: &AppState, id: i64) -> Result<Option<Subscription>> {
    Ok(sqlx::query_as!(
        Subscription,
        r#"SELECT id, name, url, priority, type AS kind, list_action, apply_automatically,
                  preserve_relationships, retract_automatically, override_unattributed,
                  last_synced_at, created_at
           FROM moderation_subscriptions WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?)
}

// ── ModerationSubscriptionSyncService ───────────────────────────────────────

/// `#action_from_severity`.
fn action_from_severity(list_action: Option<i32>, severity: Option<&str>) -> Option<i32> {
    match severity.unwrap_or("") {
        "" => Some(list_action.unwrap_or(REJECT)),
        "accept" | "allow" => (list_action != Some(REJECT)).then_some(ACCEPT),
        "limit" | "silence" => (list_action != Some(ACCEPT)).then_some(LIMIT),
        "block" | "reject" | "suspend" => (list_action != Some(ACCEPT)).then_some(REJECT),
        _ => None,
    }
}

/// One advisory by its domain: a list naming a domain twice has the last
/// entry, where `upsert_all` would refuse the whole list. A domain that does
/// not normalize is skipped rather than failing the list, and so is a wildcard
/// in a CSV list, as Mastodon skips one in a JSON list.
fn advisory(into: &mut BTreeMap<String, i32>, domain: Option<&str>, action: Option<i32>) {
    let (Some(domain), Some(action)) = (domain, action) else {
        return;
    };
    if domain.contains('*') {
        return;
    }
    if let Ok(domain) = normalize_domain(domain) {
        if !domain.is_empty() {
            into.insert(domain, action);
        }
    }
}

/// `#synchronize_csv_moderation_subscription!`'s reading of the body: a header
/// row when the first column is `#domain`, otherwise domains alone.
pub fn parse_csv(body: &[u8], list_action: Option<i32>) -> Result<BTreeMap<String, i32>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(body);
    let mut records = reader.records();
    let mut advisories = BTreeMap::new();
    let Some(first) = records.next().transpose()? else {
        return Ok(advisories);
    };
    let headers: Option<Vec<String>> =
        (first.get(0) == Some("#domain")).then(|| first.iter().map(str::to_owned).collect());
    let column = |record: &csv::StringRecord, name: &str| -> Option<String> {
        let at = match &headers {
            Some(headers) => headers.iter().position(|header| header == name)?,
            None if name == "#domain" => 0,
            None => return None,
        };
        record.get(at).map(|value| value.trim().to_lowercase())
    };
    let leading = headers.is_none().then_some(Ok(first));
    for record in leading
        .into_iter()
        .chain(records)
        .take(ROWS_PROCESSING_LIMIT + 1)
    {
        let record = record?;
        advisory(
            &mut advisories,
            column(&record, "#domain").as_deref(),
            action_from_severity(list_action, column(&record, "#severity").as_deref()),
        );
    }
    Ok(advisories)
}

#[derive(Deserialize)]
struct JsonEntry {
    domain: Option<String>,
    severity: Option<String>,
}

/// `#synchronize_json_moderation_subscription!`'s reading of the body.
pub fn parse_json(body: &[u8], list_action: Option<i32>) -> Result<BTreeMap<String, i32>> {
    let entries: Vec<JsonEntry> = serde_json::from_slice(body)?;
    let mut advisories = BTreeMap::new();
    for entry in entries {
        advisory(
            &mut advisories,
            entry.domain.as_deref(),
            action_from_severity(list_action, entry.severity.as_deref()),
        );
    }
    Ok(advisories)
}

/// `ModerationSubscriptionSyncService#call`: whether the list could be read,
/// through the client that keeps a URL from reaching private networks.
pub async fn sync(state: &AppState, subscription: &Subscription) -> bool {
    match try_sync(state, subscription).await {
        Ok(synced) => synced,
        Err(error) => {
            tracing::warn!(url = subscription.url, %error, "failed syncing moderation subscription");
            false
        }
    }
}

async fn try_sync(state: &AppState, subscription: &Subscription) -> Result<bool> {
    let json = subscription.kind == TYPE_JSON;
    let response = state
        .http
        .get(&subscription.url)
        .header(
            reqwest::header::ACCEPT,
            if json { "application/json" } else { "text/csv" },
        )
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        return Ok(false);
    }
    let Ok(body) = crate::preview_card::read_body(response, BODY_LIMIT, false).await else {
        return Ok(false);
    };
    let advisories = if json {
        parse_json(&body, subscription.list_action)?
    } else {
        parse_csv(&body, subscription.list_action)?
    };
    let keys: Vec<String> = advisories.keys().cloned().collect();
    let actions: Vec<i32> = advisories.values().copied().collect();
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        r#"INSERT INTO moderation_subscription_advisories
             (target_type, target_key, moderation_subscription_id, action, created_at, updated_at)
           SELECT $1, key, $2, action, now(), now()
           FROM unnest($3::text[], $4::int[]) AS list(key, action)
           ON CONFLICT (target_type, target_key, moderation_subscription_id)
           DO UPDATE SET action = EXCLUDED.action, updated_at = now()"#,
        TARGET_DOMAIN,
        subscription.id,
        &keys,
        &actions,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM moderation_subscription_advisories
           WHERE moderation_subscription_id = $1 AND target_type = $2
             AND NOT (target_key = ANY($3))"#,
        subscription.id,
        TARGET_DOMAIN,
        &keys,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE moderation_subscriptions SET last_synced_at = now(), updated_at = now() WHERE id = $1",
        subscription.id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

// ── ProcessModerationListsService ───────────────────────────────────────────

struct Retraction {
    action: i32,
    subscription_id: i64,
    retract_automatically: bool,
}

/// The block `DomainBlock.rule_for(domain)` finds, with what deciding about
/// it takes.
struct ExistingBlock {
    id: i64,
    domain: String,
    severity: i32,
    reject_media: bool,
    reject_reports: bool,
    moderation_subscription_id: Option<i64>,
}

impl ExistingBlock {
    /// `DomainBlock#stricter_than?`, for a new block of `severity` with nothing
    /// else set, as a list's are.
    fn weaker_than(&self, severity: i32) -> bool {
        stricter_than(
            severity,
            false,
            false,
            &crate::federation::moderation::DomainBlock {
                severity: self.severity,
                reject_media: self.reject_media,
                reject_reports: self.reject_reports,
            },
        )
    }
}

async fn rule_for(state: &AppState, domain: &str) -> Result<Option<ExistingBlock>> {
    Ok(sqlx::query_as!(
        ExistingBlock,
        r#"SELECT id, domain, severity AS "severity!", reject_media, reject_reports,
                  moderation_subscription_id
           FROM domain_blocks
           WHERE domain <> '' AND ($1 = domain OR $1 LIKE '%.' || domain)
           ORDER BY char_length(domain) DESC LIMIT 1"#,
        domain,
    )
    .fetch_optional(&state.db)
    .await?)
}

fn block_severity(action: i32) -> i32 {
    if action == REJECT {
        domain_severity::SUSPEND
    } else {
        domain_severity::SILENCE
    }
}

/// `ProcessModerationListsService#call`.
pub async fn process(state: &AppState) -> Result<()> {
    // What it does automatically is logged as the representative, which may
    // not have been made yet.
    crate::federation::instance_actor::representative(state).await?;
    let applicable: &[i32] = if state.instance.limited_federation_mode {
        &[ACCEPT]
    } else {
        &[REJECT, LIMIT]
    };
    let mut retractions = BTreeMap::new();
    let mut subscriptions = all(state).await?;
    // `order(priority: :desc)`: the subscriptions that win are written last.
    subscriptions.reverse();
    for subscription in &subscriptions {
        process_subscription(state, subscription, applicable, &mut retractions).await?;
    }
    sqlx::query!("DELETE FROM moderation_suggestions WHERE moderation_subscription_id IS NULL")
        .execute(&state.db)
        .await?;
    for (domain, retraction) in retractions {
        handle_retraction(state, &domain, retraction).await?;
    }
    Ok(())
}

async fn process_subscription(
    state: &AppState,
    subscription: &Subscription,
    applicable: &[i32],
    retractions: &mut BTreeMap<String, Retraction>,
) -> Result<()> {
    let priority = subscription.priority.unwrap_or(0);
    // Each advisory with no `superseding_advisories`, and whether it has
    // `conflicting_advisories`.
    let rows = sqlx::query!(
        r#"SELECT a.target_key, a.action,
             EXISTS (
               SELECT 1 FROM moderation_subscription_advisories o
               JOIN moderation_subscriptions s ON s.id = o.moderation_subscription_id
               WHERE o.target_type = a.target_type AND o.target_key = a.target_key
                 AND o.action <> a.action AND s.priority <= $3
                 AND o.moderation_subscription_id <> a.moderation_subscription_id
             ) AS "conflicting!"
           FROM moderation_subscription_advisories a
           WHERE a.moderation_subscription_id = $1 AND a.action = ANY($2)
             AND NOT EXISTS (
               SELECT 1 FROM moderation_subscription_advisories o
               JOIN moderation_subscriptions s ON s.id = o.moderation_subscription_id
               WHERE o.target_type = a.target_type AND o.target_key = a.target_key
                 AND o.action = a.action AND s.priority < $3
             )
           ORDER BY a.id"#,
        subscription.id,
        applicable,
        priority,
    )
    .fetch_all(&state.db)
    .await?;

    let mut kept_keys = vec![];
    let mut kept_actions = vec![];
    let with_follows: HashSet<String> =
        if subscription.apply_automatically && subscription.preserve_relationships {
            let rejected: Vec<String> = rows
                .iter()
                .filter(|row| row.action == REJECT)
                .map(|row| row.target_key.clone())
                .collect();
            // `Instance.with_domain_follows`.
            sqlx::query_scalar!(
                r#"SELECT DISTINCT a.domain AS "domain!" FROM accounts a
                   WHERE a.domain = ANY($1) AND (
                     EXISTS (SELECT 1 FROM follows f WHERE f.account_id = a.id)
                     OR EXISTS (SELECT 1 FROM follows f WHERE f.target_account_id = a.id)
                   )"#,
                &rejected,
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect()
        } else {
            HashSet::new()
        };
    for row in rows {
        let applied = subscription.apply_automatically
            && !(row.action == REJECT && with_follows.contains(&row.target_key))
            && !row.conflicting
            && apply_automatic_advisory(state, subscription, &row.target_key, row.action).await?;
        if !applied {
            kept_keys.push(row.target_key);
            kept_actions.push(row.action);
        }
    }

    // 2. Upsert the remaining suggestions; `state` stays as it is, so one
    // dismissed stays dismissed.
    sqlx::query!(
        r#"INSERT INTO moderation_suggestions
             (target_type, target_key, action, moderation_subscription_id, created_at, updated_at)
           SELECT $1, key, action, $2, now(), now()
           FROM unnest($3::text[], $4::int[]) AS kept(key, action)
           ON CONFLICT (target_type, target_key, action)
           DO UPDATE SET moderation_subscription_id = EXCLUDED.moderation_subscription_id,
                         updated_at = now()"#,
        TARGET_DOMAIN,
        subscription.id,
        &kept_keys,
        &kept_actions,
    )
    .execute(&state.db)
    .await?;

    // 3. Orphan the suggestions this subscription no longer stands behind.
    sqlx::query!(
        r#"UPDATE moderation_suggestions s
           SET moderation_subscription_id = NULL, updated_at = now()
           WHERE s.moderation_subscription_id = $1 AND s.action <> $2
             AND NOT EXISTS (
               SELECT 1 FROM unnest($3::text[], $4::int[]) AS kept(key, action)
               WHERE kept.key = s.target_key AND kept.action = s.action
             )"#,
        subscription.id,
        RETRACT,
        &kept_keys,
        &kept_actions,
    )
    .execute(&state.db)
    .await?;

    // 4. `collect_retractions!`: what this subscription applied and no longer
    // lists.
    let allows = sqlx::query_scalar!(
        r#"SELECT domain FROM domain_allows
           WHERE moderation_subscription_id = $1 AND domain NOT IN (
             SELECT target_key FROM moderation_subscription_advisories
             WHERE moderation_subscription_id = $1 AND action = $2
           )"#,
        subscription.id,
        ACCEPT,
    )
    .fetch_all(&state.db)
    .await?;
    for domain in allows {
        retractions.insert(
            domain,
            Retraction {
                action: ACCEPT,
                subscription_id: subscription.id,
                retract_automatically: subscription.retract_automatically,
            },
        );
    }
    let blocks = sqlx::query!(
        r#"SELECT domain, severity AS "severity!" FROM domain_blocks
           WHERE moderation_subscription_id = $1 AND severity <> $2 AND domain NOT IN (
             SELECT target_key FROM moderation_subscription_advisories
             WHERE moderation_subscription_id = $1 AND action = ANY($3)
           )"#,
        subscription.id,
        domain_severity::NOOP,
        &[LIMIT, REJECT],
    )
    .fetch_all(&state.db)
    .await?;
    for block in blocks {
        retractions.insert(
            block.domain,
            Retraction {
                action: if block.severity == domain_severity::SUSPEND {
                    REJECT
                } else {
                    LIMIT
                },
                subscription_id: subscription.id,
                retract_automatically: subscription.retract_automatically,
            },
        );
    }
    Ok(())
}

/// `#apply_automatic_advisory!`: whether the advisory no longer needs a
/// suggestion. Unlike Mastodon's, one already in force changes nothing: it is
/// not logged and its block not run again each time the lists are processed,
/// and an allow already there does not raise.
async fn apply_automatic_advisory(
    state: &AppState,
    subscription: &Subscription,
    domain: &str,
    action: i32,
) -> Result<bool> {
    if action == ACCEPT {
        let created = sqlx::query_scalar!(
            r#"INSERT INTO domain_allows (domain, moderation_subscription_id, created_at, updated_at)
               VALUES ($1, $2, now(), now())
               ON CONFLICT (domain) DO NOTHING RETURNING id"#,
            domain,
            subscription.id,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(id) = created {
            log_for_subscription(
                &state.db,
                REPRESENTATIVE_ID,
                "create",
                &Target::domain_allow(id, domain),
                Some(subscription.id),
            )
            .await?;
        }
        return Ok(true);
    }
    let severity = block_severity(action);
    let (id, update) = match rule_for(state, domain).await? {
        Some(existing) if existing.domain == domain => {
            let managed = existing.moderation_subscription_id == Some(subscription.id);
            let takeable =
                existing.moderation_subscription_id.is_none() && subscription.override_unattributed;
            if !(managed || takeable) {
                return Ok(false);
            }
            if managed && existing.severity == severity {
                return Ok(true);
            }
            // Only allow downgrades if retracting automatically.
            if !existing.weaker_than(severity) && !subscription.retract_automatically {
                return Ok(false);
            }
            sqlx::query!(
                r#"UPDATE domain_blocks
                   SET moderation_subscription_id = $2, severity = $3, updated_at = now()
                   WHERE id = $1"#,
                existing.id,
                subscription.id,
                severity,
            )
            .execute(&state.db)
            .await?;
            (existing.id, true)
        }
        // Skip if it would downgrade a wider existing block.
        Some(existing) if !existing.weaker_than(severity) => return Ok(false),
        _ => (
            insert_block(state, domain, severity, subscription.id).await?,
            false,
        ),
    };
    log_for_subscription(
        &state.db,
        REPRESENTATIVE_ID,
        if update { "update" } else { "create" },
        &Target::domain_block(id, domain),
        Some(subscription.id),
    )
    .await?;
    spawn_block(state, id, false).await;
    Ok(true)
}

async fn insert_block(
    state: &AppState,
    domain: &str,
    severity: i32,
    subscription_id: i64,
) -> Result<i64> {
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO domain_blocks
             (domain, severity, moderation_subscription_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           RETURNING id"#,
        domain,
        severity,
        subscription_id,
    )
    .fetch_one(&state.db)
    .await?)
}

/// One of `#handle_retractions!`. Mastodon's raises for an allow, whichever
/// way it goes, looking only for a block; an allow is treated as a block is.
async fn handle_retraction(state: &AppState, domain: &str, retraction: Retraction) -> Result<()> {
    let suggestion = sqlx::query!(
        r#"SELECT id, moderation_subscription_id FROM moderation_suggestions
           WHERE target_type = $1 AND target_key = $2 AND action = $3"#,
        TARGET_DOMAIN,
        domain,
        retraction.action,
    )
    .fetch_optional(&state.db)
    .await?;
    let allow = retraction.action == ACCEPT;
    if let Some(suggestion) = suggestion {
        // Another subscription still stands behind it, and takes it over.
        if allow {
            sqlx::query!(
                "UPDATE domain_allows SET moderation_subscription_id = $2, updated_at = now() WHERE domain = $1",
                domain,
                suggestion.moderation_subscription_id,
            )
            .execute(&state.db)
            .await?;
        } else {
            sqlx::query!(
                "UPDATE domain_blocks SET moderation_subscription_id = $2, updated_at = now() WHERE domain = $1",
                domain,
                suggestion.moderation_subscription_id,
            )
            .execute(&state.db)
            .await?;
        }
        sqlx::query!(
            "DELETE FROM moderation_suggestions WHERE id = $1",
            suggestion.id
        )
        .execute(&state.db)
        .await?;
    } else if retraction.retract_automatically {
        remove_policy(state, allow, domain, REPRESENTATIVE_ID).await?;
    } else {
        sqlx::query!(
            r#"INSERT INTO moderation_suggestions
                 (target_type, target_key, action, moderation_subscription_id, created_at, updated_at)
               VALUES ($1, $2, $3, $4, now(), now())
               ON CONFLICT (target_type, target_key, action)
               DO UPDATE SET moderation_subscription_id = EXCLUDED.moderation_subscription_id,
                             updated_at = now()"#,
            TARGET_DOMAIN,
            domain,
            RETRACT,
            retraction.subscription_id,
        )
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

/// Lift the allow or block on `domain`, logged with the subscription that had
/// it. False when there is none.
async fn remove_policy(state: &AppState, allow: bool, domain: &str, actor: i64) -> Result<bool> {
    if allow {
        let Some(row) = sqlx::query!(
            "SELECT id, moderation_subscription_id FROM domain_allows WHERE domain = $1",
            domain,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(false);
        };
        unallow_domain(state, row.id, domain).await?;
        log_for_subscription(
            &state.db,
            actor,
            "destroy",
            &Target::domain_allow(row.id, domain),
            row.moderation_subscription_id,
        )
        .await?;
    } else {
        let Some(row) = sqlx::query!(
            "SELECT id, moderation_subscription_id FROM domain_blocks WHERE domain = $1",
            domain,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(false);
        };
        log_for_subscription(
            &state.db,
            actor,
            "destroy",
            &Target::domain_block(row.id, domain),
            row.moderation_subscription_id,
        )
        .await?;
        crate::moderation::domain_block::unblock(state, row.id).await?;
    }
    Ok(true)
}

// ── ApplyModerationSuggestionService ────────────────────────────────────────

/// What `ApplyModerationSuggestionService#call` came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Applied,
    Retracted,
    UnableToApply,
    DowngradeConfirmationRequired,
    RejectConfirmationRequired,
}

impl Applied {
    pub fn name(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Retracted => "retracted",
            Self::UnableToApply => "unable_to_apply",
            Self::DowngradeConfirmationRequired => "downgrade_confirmation_required",
            Self::RejectConfirmationRequired => "reject_confirmation_required",
        }
    }
}

pub struct Suggestion {
    pub id: i64,
    pub target_key: String,
    pub action: i32,
    pub moderation_subscription_id: Option<i64>,
}

pub async fn find_suggestion(state: &AppState, id: i64) -> Result<Option<Suggestion>> {
    Ok(sqlx::query_as!(
        Suggestion,
        r#"SELECT id, target_key, action, moderation_subscription_id
           FROM moderation_suggestions WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?)
}

/// `ModerationSuggestion#mark_as_applied!`.
async fn mark_as_applied(state: &AppState, suggestion: &Suggestion) -> Result<()> {
    sqlx::query!(
        r#"UPDATE moderation_suggestions
           SET state = CASE WHEN id = $1 THEN $2::int ELSE $3::int END, updated_at = now()
           WHERE target_type = $4 AND target_key = $5"#,
        suggestion.id,
        STATE_APPLIED,
        STATE_DISMISSED,
        TARGET_DOMAIN,
        suggestion.target_key,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `ApplyModerationSuggestionService#call`, on behalf of `actor`. The service
/// as released calls methods that do not exist for blocks and retractions;
/// this is what they evidently mean.
pub async fn apply(
    state: &AppState,
    suggestion: &Suggestion,
    actor: i64,
    confirm_downgrade: bool,
    confirm: bool,
) -> Result<Applied> {
    let domain = suggestion.target_key.as_str();
    let subscription = suggestion.moderation_subscription_id;
    match suggestion.action {
        ACCEPT => {
            let created = sqlx::query_scalar!(
                r#"INSERT INTO domain_allows (domain, moderation_subscription_id, created_at, updated_at)
                   VALUES ($1, $2, now(), now())
                   ON CONFLICT (domain) DO NOTHING RETURNING id"#,
                domain,
                subscription,
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(id) = created {
                log_for_subscription(
                    &state.db,
                    actor,
                    "create",
                    &Target::domain_allow(id, domain),
                    subscription,
                )
                .await?;
            }
            mark_as_applied(state, suggestion).await?;
            Ok(Applied::Applied)
        }
        RETRACT => {
            let limited = state.instance.limited_federation_mode;
            if !remove_policy(state, limited, domain, actor).await? {
                remove_policy(state, !limited, domain, actor).await?;
            }
            mark_as_applied(state, suggestion).await?;
            Ok(Applied::Retracted)
        }
        action => apply_block(state, suggestion, action, actor, confirm_downgrade, confirm).await,
    }
}

/// `#apply_moderation_suggestion!`.
async fn apply_block(
    state: &AppState,
    suggestion: &Suggestion,
    action: i32,
    actor: i64,
    confirm_downgrade: bool,
    confirm: bool,
) -> Result<Applied> {
    let domain = suggestion.target_key.as_str();
    let subscription = suggestion.moderation_subscription_id;
    let severity = block_severity(action);
    let existing = rule_for(state, domain).await?;
    let same = existing.as_ref().filter(|e| e.domain == domain);
    // We can't create a laxer block for a subdomain than we have for a domain.
    if let Some(wider) = existing.as_ref().filter(|e| e.domain != domain) {
        if !wider.weaker_than(severity) {
            return Ok(Applied::UnableToApply);
        }
    }
    if let Some(same) = same {
        if !same.weaker_than(severity) && !confirm_downgrade {
            return Ok(Applied::DowngradeConfirmationRequired);
        }
    }
    // A new block, or one whose severity changes, needs confirming to suspend.
    let severity_changed = same.is_none_or(|same| same.severity != severity);
    if severity == domain_severity::SUSPEND && severity_changed && !confirm {
        return Ok(Applied::RejectConfirmationRequired);
    }
    let (id, update) = match same {
        // `assign_attributes` of a new block's: what it does not set goes back
        // to its default.
        Some(same) => {
            sqlx::query!(
                r#"UPDATE domain_blocks
                   SET severity = $2, moderation_subscription_id = $3, reject_media = false,
                       reject_reports = false, private_comment = NULL, public_comment = NULL,
                       obfuscate = false, updated_at = now()
                   WHERE id = $1"#,
                same.id,
                severity,
                subscription,
            )
            .execute(&state.db)
            .await?;
            (same.id, severity_changed)
        }
        None => {
            let id = sqlx::query_scalar!(
                r#"INSERT INTO domain_blocks
                     (domain, severity, moderation_subscription_id, created_at, updated_at)
                   VALUES ($1, $2, $3, now(), now())
                   RETURNING id"#,
                domain,
                severity,
                subscription,
            )
            .fetch_one(&state.db)
            .await?;
            (id, false)
        }
    };
    log_for_subscription(
        &state.db,
        actor,
        if update { "update" } else { "create" },
        &Target::domain_block(id, domain),
        subscription,
    )
    .await?;
    spawn_block(state, id, update).await;
    mark_as_applied(state, suggestion).await?;
    Ok(Applied::Applied)
}

/// `Admin::ModerationSubscriptionsController#destroy`: with its list emptied,
/// processing retracts what it applied, and then it can go.
pub async fn destroy(state: &AppState, subscription: &Subscription, actor: i64) -> Result<()> {
    sqlx::query!(
        "DELETE FROM moderation_subscription_advisories WHERE moderation_subscription_id = $1",
        subscription.id,
    )
    .execute(&state.db)
    .await?;
    process(state).await?;
    sqlx::query!(
        "DELETE FROM moderation_suggestions WHERE moderation_subscription_id = $1",
        subscription.id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        "DELETE FROM moderation_subscriptions WHERE id = $1",
        subscription.id,
    )
    .execute(&state.db)
    .await?;
    log_for_subscription(
        &state.db,
        actor,
        "destroy",
        &Target::moderation_subscription(
            subscription.id,
            subscription.name.clone().unwrap_or_default(),
        ),
        None,
    )
    .await?;
    Ok(())
}

// ── Scheduler::ModerationListSynchronizationScheduler ───────────────────────

/// `Scheduler::ModerationListSynchronizationScheduler#perform`.
pub async fn run_scheduled(state: &AppState) -> Result<()> {
    if !state.instance.feature_enabled(FEATURE) {
        return Ok(());
    }
    let mut any = false;
    for subscription in all(state).await? {
        any |= sync(state, &subscription).await;
    }
    if !any {
        return Ok(());
    }
    process(state).await?;
    mail_suggestions(state).await
}

/// `#mail_suggestions!`.
async fn mail_suggestions(state: &AppState) -> Result<()> {
    let any_new = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM moderation_suggestions WHERE state = $1) AS "e!""#,
        STATE_NEW,
    )
    .fetch_one(&state.db)
    .await?;
    if !any_new {
        return Ok(());
    }
    let moderators =
        crate::push::accounts_who_can(state, &[crate::moderation::role::flag::MANAGE_FEDERATION])
            .await?;
    for account_id in moderators {
        let Some(recipient) = sqlx::query!(
            "SELECT email, settings FROM users WHERE account_id = $1",
            account_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            continue;
        };
        if !crate::accounts::user_setting_bool(
            recipient.settings.as_deref(),
            "notification_emails.moderation_suggestions",
            true,
        ) {
            continue;
        }
        if let Err(error) = state
            .mailer()
            .send_new_moderation_suggestions(&recipient.email, &state.instance.domain)
            .await
        {
            tracing::error!(%error, "could not mail moderation suggestions");
        }
    }
    sqlx::query!(
        "UPDATE moderation_suggestions SET state = $2, updated_at = now() WHERE state = $1",
        STATE_NEW,
        STATE_MAILED,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

// ── What moderators read ─────────────────────────────────────────────────────

/// `set_current_state_by_target`.
pub async fn current_states(state: &AppState, domains: &[String]) -> Result<HashMap<String, i32>> {
    if state.instance.limited_federation_mode {
        let allowed = sqlx::query_scalar!(
            "SELECT domain FROM domain_allows WHERE domain = ANY($1)",
            domains,
        )
        .fetch_all(&state.db)
        .await?;
        let allowed: HashSet<String> = allowed.into_iter().collect();
        return Ok(domains
            .iter()
            .map(|d| (d.clone(), if allowed.contains(d) { ACCEPT } else { REJECT }))
            .collect());
    }
    let blocks = sqlx::query!(
        r#"SELECT domain, severity AS "severity!" FROM domain_blocks WHERE domain = ANY($1)"#,
        domains,
    )
    .fetch_all(&state.db)
    .await?;
    let mut states: HashMap<String, i32> = domains.iter().map(|d| (d.clone(), ACCEPT)).collect();
    for block in blocks {
        let action = match block.severity {
            domain_severity::SUSPEND => REJECT,
            domain_severity::SILENCE => LIMIT,
            _ => ACCEPT,
        };
        states.insert(block.domain, action);
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_csv_with_headers_reads_its_severity() {
        let body = b"#domain,#severity,#reject_media\nBad.Example, Suspend ,true\nmeh.example,silence,\nfine.example,accept,\n";
        let got = parse_csv(body, None).unwrap();
        assert_eq!(got.get("bad.example"), Some(&REJECT));
        assert_eq!(got.get("meh.example"), Some(&LIMIT));
        assert_eq!(got.get("fine.example"), Some(&ACCEPT));
    }

    #[test]
    fn a_csv_without_headers_is_domains_alone() {
        let got = parse_csv(b"one.example\ntwo.example\n", None).unwrap();
        assert_eq!(got.get("one.example"), Some(&REJECT));
        assert_eq!(got.get("two.example"), Some(&REJECT));
        let got = parse_csv(b"one.example\n", Some(ACCEPT)).unwrap();
        assert_eq!(got.get("one.example"), Some(&ACCEPT));
    }

    #[test]
    fn a_list_action_ignores_the_other_kind() {
        let body = b"#domain,#severity\nbad.example,suspend\nfine.example,allow\n";
        let block_list = parse_csv(body, Some(REJECT)).unwrap();
        assert_eq!(block_list.len(), 1);
        assert_eq!(block_list.get("bad.example"), Some(&REJECT));
        let allow_list = parse_csv(body, Some(ACCEPT)).unwrap();
        assert_eq!(allow_list.len(), 1);
        assert_eq!(allow_list.get("fine.example"), Some(&ACCEPT));
    }

    #[test]
    fn json_skips_wildcards() {
        let body = br#"[{"domain":"*.example"},{"domain":"bad.example","severity":"limit"}]"#;
        let got = parse_json(body, None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got.get("bad.example"), Some(&LIMIT));
        assert!(parse_json(b"{}", None).is_err());
    }
}
