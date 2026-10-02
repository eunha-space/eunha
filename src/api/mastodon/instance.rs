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

/// `Api::V1::Instances::PrivacyPoliciesController`: `PrivacyPolicy.current`.
pub async fn get_privacy_policy(state: AppState) -> AppResult<Json<crate::privacy_policy::Rest>> {
    let policy = crate::privacy_policy::current(&state).await?;
    Ok(Json(crate::privacy_policy::serialize(&state, &policy)))
}

// ── GET /api/v1/instance/extended_description ────────────────────────────

/// `ExtendedDescription.current`: the `site_extended_description` setting,
/// rendered as Markdown and dated when it was saved. Until one is saved, the
/// configured `description`, as it was served before the setting was read.
pub async fn get_extended_description(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<ExtendedDescription>> {
    let custom = sqlx::query!(
        "SELECT value, updated_at FROM settings WHERE var = 'site_extended_description'"
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = custom else {
        return Ok(Json(ExtendedDescription {
            updated_at: Some(super::convert::mastodon_date(chrono::Utc::now())),
            content: instance.description.clone(),
        }));
    };
    let text = row
        .value
        .as_deref()
        .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(raw).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    // `custom&.value.present?`, else an empty description with no date.
    Ok(Json(if text.trim().is_empty() {
        ExtendedDescription {
            updated_at: None,
            content: String::new(),
        }
    } else {
        ExtendedDescription {
            updated_at: row.updated_at.map(super::convert::mastodon_date),
            content: crate::markdown::render_html(&text),
        }
    }))
}

// ── GET /api/v1/instance ──────────────────────────────────────────────────

pub async fn get_instance_v1(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<InstanceV1>> {
    let streaming_url = format!("wss://{}/api/v1/streaming", instance.domain);
    let (user_count, status_count, domain_count) = fetch_stats(&state).await;
    let settings = crate::settings::Snapshot::load(&state).await;
    let contact_account = fetch_contact_account(&state, &settings).await;
    let registrations = settings.registrations_mode(&instance);

    let base_url = format!("https://{}", instance.domain);
    let thumbnail = crate::site_uploads::find(&state, "thumbnail").await?;
    Ok(Json(InstanceV1 {
        uri: instance.domain.clone(),
        title: settings.site_title(&instance),
        short_description: settings.site_short_description(&instance),
        description: settings.site_description(&instance),
        email: settings.site_contact_email(&instance),
        version: crate::version::compatible_string(),
        urls: InstanceV1Urls {
            streaming_api: streaming_url,
        },
        stats: InstanceV1Stats {
            user_count,
            status_count,
            domain_count,
        },
        thumbnail: thumbnail
            .and_then(|t| t.url(&state, "@1x"))
            .or_else(|| instance.icon_url.clone())
            .unwrap_or_else(|| format!("{base_url}/instance-thumbnail.png")),
        languages: vec!["ko".to_string(), "en".to_string()],
        registrations: registrations.enabled(),
        approval_required: registrations.approval_required(),
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
/// is off or the instance is in limited federation mode.
pub async fn get_peers(state: AppState) -> AppResult<Json<Vec<String>>> {
    require_enabled_api(&state, "peers_api_enabled").await?;
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

/// `require_enabled_api!` of the peers, peer search and activity controllers:
/// a 404 unless `setting` is on and the instance is not in limited
/// federation mode, which keeps who it federates with to itself.
async fn require_enabled_api(state: &AppState, setting: &str) -> AppResult<()> {
    if state.instance.limited_federation_mode || !crate::settings::boolean(state, setting).await {
        return Err(AppError::NotFound);
    }
    Ok(())
}

// ── GET /api/v1/peers/search ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PeersSearchParams {
    pub q: Option<String>,
}

/// `Api::V1::Peers::SearchController#index` (crate::search::peers).
pub async fn search_peers(
    state: AppState,
    Query(params): Query<PeersSearchParams>,
) -> AppResult<Json<Option<Vec<String>>>> {
    require_enabled_api(&state, "peers_api_enabled").await?;
    Ok(Json(
        crate::search::peers::search(&state, params.q.as_deref()).await?,
    ))
}

// ── GET /api/v1/instance/terms_of_service ────────────────────────────────

/// `Api::V1::Instances::TermsOfServiceController#index`: `TermsOfService.current`,
/// a 404 when there is none.
pub async fn get_terms_of_service(
    state: AppState,
) -> AppResult<Json<crate::terms_of_service::Rest>> {
    let tos = crate::terms_of_service::current(&state)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(
        crate::terms_of_service::serialize(&state, &tos).await?,
    ))
}

// ── GET /api/v1/instance/terms_of_service/{date} ─────────────────────────

/// `#show`: `TermsOfService.published.find_by!(effective_date:)`.
pub async fn get_terms_of_service_by_date(
    state: AppState,
    Path(date): Path<String>,
) -> AppResult<Json<crate::terms_of_service::Rest>> {
    let tos = crate::terms_of_service::published_by_date(&state, &date).await?;
    Ok(Json(
        crate::terms_of_service::serialize(&state, &tos).await?,
    ))
}

pub async fn get_instance_v2(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<InstanceV2>> {
    let streaming_url = format!("wss://{}/api/v1/streaming", instance.domain);
    let base_url = format!("https://{}", instance.domain);
    let (_, _, _) = fetch_stats(&state).await;
    let settings = crate::settings::Snapshot::load(&state).await;
    let contact_account = fetch_contact_account(&state, &settings).await;
    let registrations = settings.registrations_mode(&instance);
    let thumbnail = crate::site_uploads::find(&state, "thumbnail").await?;
    let app_icon = crate::site_uploads::find(&state, "app_icon").await?;
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
        title: settings.site_title(&instance),
        version: crate::version::compatible_string(),
        source_url: "https://github.com/limeburst/eunha".to_string(),
        description: settings.site_short_description(&instance),
        usage: InstanceUsage {
            users: InstanceUsageUsers { active_month },
        },
        thumbnail: match &thumbnail {
            Some(upload) => InstanceThumbnail {
                url: upload.url(&state, "@1x").unwrap_or_default(),
                blurhash: upload.blurhash.clone(),
                versions: Some(serde_json::json!({
                    "@1x": upload.url(&state, "@1x"),
                    "@2x": upload.url(&state, "@2x"),
                })),
                description: Some(settings.string("thumbnail_description")),
            },
            None => InstanceThumbnail {
                url: instance
                    .icon_url
                    .clone()
                    .unwrap_or_else(|| format!("{base_url}/instance-thumbnail.png")),
                blurhash: None,
                versions: None,
                description: None,
            },
        },
        // `SiteUpload::ANDROID_ICON_SIZES` of the uploaded app icon, or else
        // the configured icon.
        icon: match &app_icon {
            Some(upload) => crate::site_uploads::ANDROID_ICON_SIZES
                .iter()
                .map(|size| {
                    serde_json::json!({
                        "src": upload.url(&state, &size.to_string()),
                        "size": format!("{size}x{size}"),
                    })
                })
                .collect(),
            None => instance
                .icon_url
                .as_ref()
                .map(|url| {
                    vec![
                        serde_json::json!({ "src": url, "size": "192x192" }),
                        serde_json::json!({ "src": url, "size": "512x512" }),
                    ]
                })
                .unwrap_or_default(),
        },
        languages: vec!["ko".to_string(), "en".to_string()],
        configuration: InstanceConfiguration {
            urls: InstanceUrls {
                streaming: streaming_url,
                // `InstancePresenter#status_page_url`.
                status: Some(settings.string("status_page_url")).filter(|u| !u.is_empty()),
                about: Some(format!("{base_url}/about")),
                // `privacy_policy_url`: there is always a policy, if only
                // the one Mastodon ships.
                privacy_policy: Some(format!("{base_url}/privacy-policy")),
                // `terms_of_service_url` when `TermsOfService.current` exists.
                terms_of_service: crate::terms_of_service::current(&state)
                    .await?
                    .map(|_| format!("{base_url}/terms-of-service")),
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
            translation: TranslationConfiguration {
                enabled: state.instance.translation.configured(),
            },
            // The `*_feed_access` settings, the link feeds sharing the
            // hashtag feeds'.
            timelines_access: TimelinesAccess {
                live_feeds: TimelineAccessControl {
                    local: crate::settings::string(&state, "local_live_feed_access").await,
                    remote: crate::settings::string(&state, "remote_live_feed_access").await,
                },
                hashtag_feeds: TimelineAccessControl {
                    local: crate::settings::string(&state, "local_topic_feed_access").await,
                    remote: crate::settings::string(&state, "remote_topic_feed_access").await,
                },
                trending_link_feeds: TimelineAccessControl {
                    local: crate::settings::string(&state, "local_topic_feed_access").await,
                    remote: crate::settings::string(&state, "remote_topic_feed_access").await,
                },
            },
            limited_federation: state.instance.limited_federation_mode,
        },
        registrations: InstanceRegistrations {
            enabled: registrations.enabled(),
            approval_required: registrations.approval_required(),
            reason_required: registrations.approval_required()
                && settings.boolean("require_invite_text"),
            min_age: crate::settings::min_age(&state.db).await,
            // `registrations_message`, only while registrations are closed.
            message: if registrations.enabled() {
                None
            } else {
                Some(settings.string("closed_registrations_message"))
                    .filter(|m| !m.trim().is_empty())
                    .map(|m| crate::markdown::render_without_images(&m))
            },
            url: None,
        },
        contact: InstanceContact {
            email: settings.site_contact_email(&instance),
            account: contact_account,
        },
        rules: crate::moderation::rules::serialize(&state, None).await?,
        api_versions: serde_json::json!({ "mastodon": 9 }),
        wrapstodon: None,
    }))
}

// ── GET /api/v1/instance/activity ────────────────────────────────────────

pub async fn get_instance_activity(state: AppState) -> AppResult<Json<Vec<serde_json::Value>>> {
    require_enabled_api(&state, "activity_api_enabled").await?;
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

/// `InstancePresenter::ContactPresenter#account`: the local account
/// `site_contact_username` names. Until one is saved, the local account with
/// the highest role, as eunha picked before it read the setting.
async fn fetch_contact_account(
    state: &AppState,
    settings: &crate::settings::Snapshot,
) -> Option<super::types::Account> {
    let account = if settings.stored("site_contact_username") {
        let configured = settings.string("site_contact_username");
        let username = configured.trim().trim_start_matches('@');
        let username = username.split('@').next().unwrap_or("");
        if username.is_empty() {
            return None;
        }
        sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE domain IS NULL AND lower(username) = lower($1)",
            username,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()?
    } else {
        sqlx::query_as!(
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
        .flatten()?
    };
    let mut api = super::convert::account_from_db(&state.urls, &account);
    api.emojis = super::accounts::fetch_account_emojis(state, &account).await;
    api.roles = {
        let m = super::accounts::batch_account_roles(state, std::slice::from_ref(&account)).await;
        m.get(&account.id).cloned().unwrap_or_default()
    };
    super::accounts::apply_account_stats(state, &mut api, account.id).await;
    Some(api)
}
