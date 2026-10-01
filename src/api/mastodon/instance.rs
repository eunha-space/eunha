use super::types::*;
use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};
use axum::{
    extract::{Extension, Path, Query},
    Json,
};
use serde::Deserialize;

// ── GET /api/v1/instance/translation_languages ───────────────────────────
// Returns empty object — translation is not supported.

pub async fn get_translation_languages() -> Json<serde_json::Value> {
    Json(serde_json::json!({}))
}

// ── GET /api/v1/instance/languages ───────────────────────────────────────

/// `LanguagesHelper::SUPPORTED_LOCALES`, each as `REST::LanguageSerializer`.
pub async fn get_instance_languages() -> Json<Vec<serde_json::Value>> {
    Json(
        crate::languages::SUPPORTED_LOCALES
            .iter()
            .map(|(code, name, _)| serde_json::json!({ "code": code, "name": name }))
            .collect(),
    )
}

// ── GET /api/v1/instance/domain_blocks ───────────────────────────────────

/// `User#functional_or_moved?`: confirmed, approved, not disabled, and its
/// account neither unavailable nor a memorial.
async fn functional_or_moved(state: &AppState, auth: Option<&AuthenticatedUser>) -> bool {
    let Some(auth) = auth else {
        return false;
    };
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM users u JOIN accounts a ON a.id = u.account_id
             WHERE u.account_id = $1 AND u.confirmed_at IS NOT NULL AND u.approved
               AND NOT u.disabled AND a.suspended_at IS NULL
               AND a.requested_deletion_at IS NULL AND NOT a.memorial
           ) AS "e!""#,
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// `Api::V1::Instances::DomainBlocksController`: the domains limited or
/// suspended here, to everyone or to signed-in users as the
/// `show_domain_blocks` setting says (a 404 when it is `disabled`), with their
/// public comment as `show_domain_blocks_rationale` says.
pub async fn get_instance_domain_blocks(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    let auth = auth.map(|Extension(a)| a);
    let visible_to = |setting: String, functional: bool| match setting.as_str() {
        "all" => true,
        "users" => functional,
        _ => false,
    };
    let functional = functional_or_moved(&state, auth.as_ref()).await;
    if !visible_to(
        crate::settings::string(&state, "show_domain_blocks").await,
        functional,
    ) {
        return Err(AppError::NotFound);
    }
    let with_comment = visible_to(
        crate::settings::string(&state, "show_domain_blocks_rationale").await,
        functional,
    );

    // `with_user_facing_limitations.by_severity`: silence and suspend, in that
    // order, then by domain.
    let rows = sqlx::query!(
        r#"SELECT domain, severity, public_comment, obfuscate FROM domain_blocks
           WHERE COALESCE(severity, 0) IN (0, 1)
           ORDER BY COALESCE(severity, 0), domain"#
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| {
                serde_json::json!({
                    "domain": if r.obfuscate { public_domain(&r.domain) } else { r.domain.clone() },
                    "digest": domain_digest(&r.domain),
                    "severity": crate::db::models::domain_severity::to_str(r.severity),
                    "comment": if with_comment { r.public_comment } else { None },
                })
            })
            .collect(),
    ))
}

fn domain_digest(domain: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(domain.as_bytes());
    hex::encode(h.finalize())
}

/// `DomainBlock#public_domain`: the middle of an obfuscated domain starred
/// out, its dots kept.
fn public_domain(domain: &str) -> String {
    let chars: Vec<char> = domain.chars().collect();
    let length = chars.len();
    let visible_ratio = length / 4;
    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            if i > visible_ratio && i < length - visible_ratio && c != '.' {
                '*'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod public_domain_tests {
    #[test]
    fn stars_the_middle_like_mastodon() {
        // `'example.com'`: length 11, visible ratio 2.
        assert_eq!(super::public_domain("example.com"), "exa****.*om");
    }
}

// ── GET /api/v1/instance/rules ────────────────────────────────────────────

/// `Rule.ordered`.
pub async fn get_instance_rules(state: AppState) -> AppResult<Json<Vec<Rule>>> {
    Ok(Json(
        crate::moderation::rules::serialize(&state, None).await?,
    ))
}

// ── GET /api/v1/instance/privacy_policy ──────────────────────────────────

pub async fn get_privacy_policy(
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<ExtendedDescription>> {
    Ok(Json(ExtendedDescription {
        updated_at: super::convert::mastodon_date(chrono::Utc::now()),
        content: instance.privacy_policy.clone(),
    }))
}

// ── GET /api/v1/instance/extended_description ────────────────────────────

pub async fn get_extended_description(
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<ExtendedDescription>> {
    Ok(Json(ExtendedDescription {
        updated_at: super::convert::mastodon_date(chrono::Utc::now()),
        content: instance.description.clone(),
    }))
}

// ── GET /api/v1/instance ──────────────────────────────────────────────────

pub async fn get_instance_v1(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<InstanceV1>> {
    let streaming_url = format!("wss://{}/api/v1/streaming", instance.domain);
    let (user_count, status_count, domain_count) = fetch_stats(&state).await;
    let contact_account = fetch_contact_account(&state).await;

    let base_url = format!("https://{}", instance.domain);
    Ok(Json(InstanceV1 {
        uri: instance.domain.clone(),
        title: instance.title.clone(),
        short_description: instance.short_description.clone(),
        description: instance.description.clone(),
        email: instance.contact_email.clone().unwrap_or_default(),
        version: crate::version::compatible_string(),
        urls: InstanceV1Urls {
            streaming_api: streaming_url,
        },
        stats: InstanceV1Stats {
            user_count,
            status_count,
            domain_count,
        },
        thumbnail: instance
            .icon_url
            .clone()
            .unwrap_or_else(|| format!("{base_url}/instance-thumbnail.png")),
        languages: vec!["ko".to_string(), "en".to_string()],
        registrations: instance.registrations_open,
        approval_required: instance.approval_required,
        invites_enabled: false,
        configuration: serde_json::json!({
            "accounts": { "max_featured_tags": 10 },
            "statuses": {
                "max_characters": 500,
                "max_media_attachments": 4,
                "characters_reserved_per_url": 23,
            },
            "media_attachments": {
                "supported_mime_types": [
                    "image/jpeg","image/png","image/gif","image/heic","image/heif",
                    "image/webp","image/avif","video/webm","video/mp4","video/quicktime",
                    "video/ogg","audio/wave","audio/wav","audio/x-wav","audio/x-pn-wave",
                    "audio/vnd.wave","audio/ogg","audio/vorbis","audio/mpeg","audio/mp3",
                    "audio/webm","audio/flac","audio/aac","audio/m4a","audio/x-m4a",
                    "audio/mp4","audio/3gpp","video/x-ms-asf"
                ],
                "image_size_limit": 16777216,
                "image_matrix_limit": 33177600,
                "video_size_limit": 103809024,
                "video_frame_rate_limit": 120,
                "video_matrix_limit": 8294400,
            },
            "polls": {
                "max_options": 4,
                "max_characters_per_option": 50,
                "min_expiration": 300,
                "max_expiration": 2629746,
            },
        }),
        contact_account,
        rules: crate::moderation::rules::serialize(&state, None).await?,
    }))
}

// ── GET /api/v1/instance/peers ────────────────────────────────────────────

/// `Api::V1::Instances::PeersController`: `Instance.searchable`, the known
/// domains (the `instances` view, computed here rather than read from the
/// materialized copy) less the blocked ones; a 404 when `peers_api_enabled`
/// is off.
pub async fn get_peers(state: AppState) -> AppResult<Json<Vec<String>>> {
    if !crate::settings::boolean(&state, "peers_api_enabled").await {
        return Err(AppError::NotFound);
    }
    let rows = sqlx::query_scalar!(
        r#"SELECT domain AS "domain!" FROM (
             SELECT domain FROM accounts WHERE domain IS NOT NULL
             UNION SELECT domain FROM domain_allows
           ) known
           WHERE domain NOT IN (SELECT domain FROM domain_blocks)
           ORDER BY domain"#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

// ── GET /api/v1/peers/search ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PeersSearchParams {
    pub q: Option<String>,
}

pub async fn search_peers(
    state: AppState,
    Query(params): Query<PeersSearchParams>,
) -> AppResult<Json<Vec<String>>> {
    let q = params.q.as_deref().unwrap_or("").trim().to_string();
    let pattern = format!("%{}%", q);
    let rows = sqlx::query_scalar!(
        "SELECT DISTINCT domain FROM accounts WHERE domain IS NOT NULL AND domain ILIKE $1 ORDER BY domain LIMIT 20",
        pattern,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .flatten()
    .collect();
    Ok(Json(rows))
}

// ── GET /api/v1/instance/terms_of_service ────────────────────────────────

pub async fn get_terms_of_service(
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<Vec<TermsOfServiceByDate>>> {
    if instance.terms_of_service.is_empty() {
        return Ok(Json(vec![]));
    }
    Ok(Json(vec![TermsOfServiceByDate {
        effective_date: "2025-01-01".to_string(),
        effective: true,
        content: instance.terms_of_service.clone(),
        succeeded_by: None,
    }]))
}

// ── GET /api/v1/instance/terms_of_service/{date} ─────────────────────────
// Mastodon supports versioned ToS by effective date. eunha has a single ToS,
// so we return it for any date, or 404 if the ToS is empty.

#[derive(Debug, serde::Serialize)]
pub struct TermsOfServiceByDate {
    pub effective_date: String,
    pub effective: bool,
    pub content: String,
    pub succeeded_by: Option<String>,
}

pub async fn get_terms_of_service_by_date(
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(date): Path<String>,
) -> AppResult<Json<TermsOfServiceByDate>> {
    if instance.terms_of_service.is_empty() {
        return Err(crate::error::AppError::NotFound);
    }
    // Validate that `date` looks like a date (YYYY-MM-DD); return 404 for other dates
    if date.len() != 10 || !date.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return Err(crate::error::AppError::NotFound);
    }
    // Only recognise the single fixed effective date
    if date != "2025-01-01" {
        return Err(crate::error::AppError::NotFound);
    }
    Ok(Json(TermsOfServiceByDate {
        effective_date: date,
        effective: true,
        content: instance.terms_of_service.clone(),
        succeeded_by: None,
    }))
}

pub async fn get_instance_v2(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<InstanceV2>> {
    let streaming_url = format!("wss://{}/api/v1/streaming", instance.domain);
    let base_url = format!("https://{}", instance.domain);
    let (_, _, _) = fetch_stats(&state).await;
    let contact_account = fetch_contact_account(&state).await;
    let active_month = sqlx::query_scalar!(
        r#"SELECT COUNT(DISTINCT s.account_id)
           FROM statuses s
           WHERE s.account_id IN (
               SELECT id FROM accounts WHERE domain IS NULL
           ) AND s.deleted_at IS NULL
             AND s.created_at > now() - interval '30 days'"#,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    Ok(Json(InstanceV2 {
        domain: instance.domain.clone(),
        title: instance.title.clone(),
        version: crate::version::compatible_string(),
        source_url: "https://github.com/limeburst/eunha".to_string(),
        description: instance.description.clone(),
        usage: InstanceUsage {
            users: InstanceUsageUsers { active_month },
        },
        thumbnail: InstanceThumbnail {
            url: instance
                .icon_url
                .clone()
                .unwrap_or_else(|| format!("{base_url}/instance-thumbnail.png")),
            blurhash: None,
            versions: None,
            description: None,
        },
        icon: instance
            .icon_url
            .as_ref()
            .map(|url| {
                vec![
                    serde_json::json!({ "src": url, "size": "192x192" }),
                    serde_json::json!({ "src": url, "size": "512x512" }),
                ]
            })
            .unwrap_or_default(),
        languages: vec!["ko".to_string(), "en".to_string()],
        configuration: InstanceConfiguration {
            urls: InstanceUrls {
                streaming: streaming_url,
                status: None,
                about: Some(format!("{base_url}/about")),
                privacy_policy: if instance.privacy_policy.is_empty() {
                    None
                } else {
                    Some(format!("{base_url}/api/v1/instance/privacy_policy"))
                },
                terms_of_service: if instance.terms_of_service.is_empty() {
                    None
                } else {
                    Some(format!("{base_url}/api/v1/instance/terms_of_service"))
                },
            },
            vapid: VapidConfiguration {
                public_key: instance.vapid_public_key.clone(),
            },
            accounts: AccountsConfiguration {
                max_featured_tags: 10,
                max_pinned_statuses: 5,
                max_profile_fields: 4,
                // Mastodon's `Account::DISPLAY_NAME_LENGTH_LIMIT`, which is 40;
                // eunha advertised 30, so a client would have refused a name
                // this server would have accepted.
                max_display_name_length: 40,
                max_note_length: 500,
                max_avatar_description_length: 150,
                max_header_description_length: 150,
                profile_field_name_limit: 255,
                profile_field_value_limit: 255,
            },
            statuses: StatusesConfiguration {
                max_characters: 500,
                max_media_attachments: 4,
                characters_reserved_per_url: 23,
            },
            media_attachments: MediaConfiguration {
                supported_mime_types: vec![
                    "image/jpeg".into(),
                    "image/png".into(),
                    "image/gif".into(),
                    "image/heic".into(),
                    "image/heif".into(),
                    "image/webp".into(),
                    "image/avif".into(),
                    "video/webm".into(),
                    "video/mp4".into(),
                    "video/quicktime".into(),
                    "video/ogg".into(),
                    "audio/wave".into(),
                    "audio/wav".into(),
                    "audio/x-wav".into(),
                    "audio/x-pn-wave".into(),
                    "audio/vnd.wave".into(),
                    "audio/ogg".into(),
                    "audio/vorbis".into(),
                    "audio/mpeg".into(),
                    "audio/mp3".into(),
                    "audio/webm".into(),
                    "audio/flac".into(),
                    "audio/aac".into(),
                    "audio/m4a".into(),
                    "audio/x-m4a".into(),
                    "audio/mp4".into(),
                    "audio/3gpp".into(),
                    "video/x-ms-asf".into(),
                ],
                // Mastodon's `MediaAttachment::MAX_DESCRIPTION_LENGTH`, raised
                // to 10,000 upstream; 1500 was the older limit. eunha does not
                // enforce a limit of its own, so advertising the smaller number
                // only made clients refuse alt text this server would accept.
                description_limit: 10_000,
                image_size_limit: 16 * 1024 * 1024,
                image_matrix_limit: 33_177_600,
                video_size_limit: 99 * 1024 * 1024,
                video_frame_rate_limit: 120,
                video_matrix_limit: 8_294_400,
            },
            polls: PollsConfiguration {
                max_options: 4,
                max_characters_per_option: 50,
                min_expiration: 300,
                max_expiration: 2_629_746,
            },
            translation: TranslationConfiguration { enabled: false },
            timelines_access: TimelinesAccess {
                live_feeds: TimelineAccessControl {
                    local: "public".into(),
                    remote: "public".into(),
                },
                hashtag_feeds: TimelineAccessControl {
                    local: "public".into(),
                    remote: "public".into(),
                },
                trending_link_feeds: TimelineAccessControl {
                    local: "public".into(),
                    remote: "public".into(),
                },
            },
            limited_federation: false,
        },
        registrations: InstanceRegistrations {
            enabled: instance.registrations_open,
            approval_required: instance.approval_required,
            reason_required: false,
            min_age: None,
            message: None,
            url: None,
        },
        contact: InstanceContact {
            email: instance.contact_email.clone().unwrap_or_default(),
            account: contact_account,
        },
        rules: crate::moderation::rules::serialize(&state, None).await?,
        api_versions: serde_json::json!({ "mastodon": 9 }),
        wrapstodon: None,
    }))
}

// ── GET /api/v1/instance/activity ────────────────────────────────────────

pub async fn get_instance_activity(state: AppState) -> AppResult<Json<Vec<serde_json::Value>>> {
    // Return 12 weeks of activity
    let rows = sqlx::query!(
        r#"SELECT
             EXTRACT(EPOCH FROM date_trunc('week', s.created_at))::bigint AS week,
             COUNT(s.id) AS statuses,
             COUNT(DISTINCT s.account_id) AS logins
           FROM statuses s
           WHERE s.account_id IN (
               SELECT id FROM accounts WHERE domain IS NULL
           ) AND s.deleted_at IS NULL
             AND s.created_at >= date_trunc('week', now()) - interval '11 weeks'
           GROUP BY date_trunc('week', s.created_at)
           ORDER BY week DESC"#,
    )
    .fetch_all(&state.db)
    .await?;

    let registrations_rows = sqlx::query!(
        r#"SELECT
             EXTRACT(EPOCH FROM date_trunc('week', a.created_at))::bigint AS week,
             COUNT(a.id) AS registrations
           FROM accounts a
           WHERE a.domain IS NULL
             AND a.created_at >= date_trunc('week', now()) - interval '11 weeks'
           GROUP BY date_trunc('week', a.created_at)"#,
    )
    .fetch_all(&state.db)
    .await?;

    // Build a map of week -> registration count
    let mut reg_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for r in registrations_rows {
        if let (Some(week), Some(count)) = (r.week, r.registrations) {
            reg_map.insert(week, count);
        }
    }

    let mut result = Vec::new();
    for r in rows {
        if let (Some(week), Some(statuses), Some(logins)) = (r.week, r.statuses, r.logins) {
            let registrations = reg_map.get(&week).copied().unwrap_or(0);
            result.push(serde_json::json!({
                "week": week.to_string(),
                "statuses": statuses.to_string(),
                "logins": logins.to_string(),
                "registrations": registrations.to_string(),
            }));
        }
    }

    Ok(Json(result))
}

async fn fetch_stats(state: &AppState) -> (i64, i64, i64) {
    let user_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM accounts WHERE domain IS NULL AND suspended_at IS NULL AND requested_deletion_at IS NULL",
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    let status_count = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(ast.statuses_count), 0)::bigint
           FROM account_stats ast
           JOIN accounts a ON a.id = ast.account_id
           WHERE a.domain IS NULL"#,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    let domain_count = sqlx::query_scalar!(
        "SELECT COUNT(DISTINCT domain) FROM accounts WHERE domain IS NOT NULL",
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    (user_count, status_count, domain_count)
}

async fn fetch_contact_account(state: &AppState) -> Option<super::types::Account> {
    let account = sqlx::query_as!(
        crate::db::models::Account,
        r#"SELECT a.* FROM accounts a
           JOIN users u ON u.account_id = a.id
           LEFT JOIN user_roles ur ON ur.id = u.role_id
           WHERE a.domain IS NULL
           ORDER BY COALESCE(ur.position, 0) DESC, a.created_at ASC
           LIMIT 1"#,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    let mut api = super::convert::account_from_db(&state.urls, &account);
    api.emojis = super::accounts::fetch_account_emojis(state, &account).await;
    api.roles = {
        let m = super::accounts::batch_account_roles(state, std::slice::from_ref(&account)).await;
        m.get(&account.id).cloned().unwrap_or_default()
    };
    super::accounts::apply_account_stats(state, &mut api, account.id).await;
    Some(api)
}
