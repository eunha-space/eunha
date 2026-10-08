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
use serde::{Deserialize, Serialize};

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

/// `User#functional_or_moved?`: confirmed, approved, not disabled, its
/// account neither unavailable nor a memorial, and no second factor missing
/// that its role requires.
async fn functional_or_moved(state: &AppState, auth: Option<&AuthenticatedUser>) -> bool {
    let Some(auth) = auth else {
        return false;
    };
    crate::user_standing::UserStanding::of_account(&state.db, auth.account_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|standing| standing.functional_or_moved())
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
    Ok(Json(crate::privacy_policy::serialize(&state, &policy)?))
}

// ── GET /api/v1/instance/extended_description ────────────────────────────

/// `ExtendedDescription.current`: the `site_extended_description` setting,
/// rendered as Markdown and dated when it was saved.
pub async fn get_extended_description(state: AppState) -> AppResult<Json<ExtendedDescription>> {
    let custom = sqlx::query!(
        "SELECT value, updated_at FROM settings WHERE var = 'site_extended_description'"
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = custom else {
        return Ok(Json(ExtendedDescription {
            updated_at: None,
            content: String::new(),
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

/// `Mastodon::Version.source_url`: where eunha's source is.
const SOURCE_URL: &str = "https://github.com/eunha-space/eunha";

/// The alt text of [`default_thumbnail_url`]'s picture, as Mastodon keeps
/// `about.default_thumbnail_description` for its own.
pub const DEFAULT_THUMBNAIL_DESCRIPTION: &str =
    "A cream-colored rabbit with one lilac ear peeks out of a mint-green planetary ring against a dark navy sky.";

/// `frontend_asset_url('images/preview.png')`: the picture an instance with
/// no thumbnail of its own is shown with, from the web frontend.
fn default_thumbnail_url(base_url: &str) -> String {
    format!("{base_url}/images/preview.png")
}

/// `Rails.configuration.x.streaming_api_base_url`: the streaming server's
/// origin, which clients add `/api/v1/streaming` to.
fn streaming_api_base_url(domain: &str) -> String {
    format!("wss://{domain}")
}

/// `InstancePresenter#languages`: `[I18n.default_locale]`.
fn languages(state: &AppState) -> Vec<String> {
    vec![state.instance.default_locale().to_string()]
}

pub async fn get_instance_v1(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
) -> AppResult<Json<InstanceV1>> {
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
            streaming_api: streaming_api_base_url(&instance.domain),
        },
        stats: InstanceV1Stats {
            user_count,
            status_count,
            domain_count,
        },
        // `full_asset_url(thumbnail.file.url(:'@1x'))`, else
        // `frontend_asset_url('images/preview.png')`.
        thumbnail: thumbnail
            .and_then(|t| t.url(&state, "@1x"))
            .unwrap_or_else(|| default_thumbnail_url(&base_url)),
        languages: languages(&state),
        registrations: registrations.enabled(),
        approval_required: registrations.approval_required(),
        // `UserRole.everyone.can?(:invite_users)`.
        invites_enabled: crate::moderation::role::everyone_can(
            &state.db,
            crate::moderation::role::flag::INVITE_USERS,
        )
        .await?,
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
    let base_url = format!("https://{}", instance.domain);
    let settings = crate::settings::Snapshot::load(&state).await;
    let contact_account = fetch_contact_account(&state, &settings).await;
    let registrations = settings.registrations_mode(&instance);
    let thumbnail = crate::site_uploads::find(&state, "thumbnail").await?;
    let app_icon = crate::site_uploads::find(&state, "app_icon").await?;
    // `usage`: `active_user_count(4)`, or none to show in limited
    // federation mode.
    let active_month = if state.instance.limited_federation_mode {
        0
    } else {
        crate::activity_tracker::active_user_count(&state, 4).await
    };

    Ok(Json(InstanceV2 {
        domain: instance.domain.clone(),
        title: settings.site_title(&instance),
        version: crate::version::compatible_string(),
        source_url: SOURCE_URL.to_string(),
        description: settings.site_short_description(&instance),
        usage: InstanceUsage {
            users: InstanceUsageUsers { active_month },
        },
        thumbnail: match &thumbnail {
            Some(upload) => serde_json::json!({
                "url": upload.url(&state, "@1x"),
                "blurhash": upload.blurhash,
                "versions": {
                    "@1x": upload.url(&state, "@1x"),
                    "@2x": upload.url(&state, "@2x"),
                },
                "description": settings.string("thumbnail_description"),
            }),
            None => serde_json::json!({
                "url": default_thumbnail_url(&base_url),
                "description": DEFAULT_THUMBNAIL_DESCRIPTION,
            }),
        },
        // Each of `SiteUpload::ANDROID_ICON_SIZES`: the uploaded app icon's,
        // else the frontend's own `icons/android-chrome-#{size}x#{size}.png`.
        icon: crate::site_uploads::ANDROID_ICON_SIZES
            .iter()
            .map(|size| {
                let src = app_icon
                    .as_ref()
                    .and_then(|upload| upload.url(&state, &size.to_string()))
                    .unwrap_or_else(|| {
                        format!("{base_url}/icons/android-chrome-{size}x{size}.png")
                    });
                serde_json::json!({ "src": src, "size": format!("{size}x{size}") })
            })
            .collect(),
        languages: languages(&state),
        configuration: InstanceConfiguration {
            urls: InstanceUrls {
                streaming: streaming_api_base_url(&instance.domain),
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
        // `Mastodon::Version.api_versions`.
        api_versions: serde_json::json!({ "mastodon": 11 }),
        wrapstodon: super::annual_reports::current_campaign(&state).await,
    }))
}

// ── GET /api/v1/instance/activity ────────────────────────────────────────

/// `Api::V1::Instances::ActivityController::WEEKS_OF_ACTIVITY`.
const WEEKS_OF_ACTIVITY: i64 = 12;

/// The `Rails.cache` key `render_with_cache` keeps the body under
/// (`"#{controller}/#{action}"`, in the cache store's `cache` namespace), raw,
/// so a Mastodon process sharing the Redis serves what eunha cached and the
/// other way round.
const ACTIVITY_CACHE_KEY: &str = "cache:api/v1/instances/activity/show";

/// `render_with_cache json: :activity, expires_in: 1.day`.
const ACTIVITY_CACHE_TTL: u64 = 24 * 60 * 60;

#[derive(Serialize)]
struct ActivityWeek {
    week: String,
    statuses: String,
    logins: String,
    registrations: String,
}

/// `Api::V1::Instances::ActivityController#show`: the last twelve weeks, each
/// from a week ago to six days after, as `ActivityTracker` counted them —
/// local public and unlisted posts, users who signed in, and sign-ups —
/// rendered once a day and served from the cache in between.
pub async fn get_instance_activity(state: AppState) -> AppResult<axum::response::Response> {
    use axum::response::IntoResponse;

    require_enabled_api(&state, "activity_api_enabled").await?;

    let cache_key = state.redis_keys.key(ACTIVITY_CACHE_KEY);
    let mut redis = state.redis.clone();
    let cached: Option<String> = redis::cmd("GET")
        .arg(&cache_key)
        .query_async(&mut redis)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let body = match cached {
        Some(body) => body,
        None => {
            let body = serde_json::to_string(&activity_weeks(&state).await?)
                .map_err(|e| AppError::Internal(e.into()))?;
            let written: redis::RedisResult<()> = redis::cmd("SET")
                .arg(&cache_key)
                .arg(&body)
                .arg("EX")
                .arg(ACTIVITY_CACHE_TTL)
                .query_async(&mut redis)
                .await;
            if let Err(error) = written {
                tracing::warn!(%error, "could not cache the instance activity");
            }
            body
        }
    };
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

/// `ActivityController#activity`.
async fn activity_weeks(state: &AppState) -> AppResult<Vec<ActivityWeek>> {
    use crate::activity_tracker::{sum, Kind, ACCOUNTS_LOCAL, LOGINS, STATUSES_LOCAL};

    let now = chrono::Utc::now();
    let mut weeks = Vec::new();
    for weeks_ago in 0..WEEKS_OF_ACTIVITY {
        // `week_edge_days(num)`: `[num.weeks.ago, num.weeks.ago + 6.days]`.
        let start_of_week = now - chrono::Duration::weeks(weeks_ago);
        let start = start_of_week.date_naive();
        let end = (start_of_week + chrono::Duration::days(6)).date_naive();
        let count = |prefix, kind| async move {
            sum(state, prefix, kind, start, end)
                .await
                .map(|n| n.to_string())
                .map_err(|e| AppError::Internal(e.into()))
        };
        weeks.push(ActivityWeek {
            week: start_of_week.timestamp().to_string(),
            statuses: count(STATUSES_LOCAL, Kind::Basic).await?,
            logins: count(LOGINS, Kind::Unique).await?,
            registrations: count(ACCOUNTS_LOCAL, Kind::Basic).await?,
        });
    }
    Ok(weeks)
}

/// `InstancePresenter#user_count`, `#status_count` and `#domain_count`.
async fn fetch_stats(state: &AppState) -> (i64, i64, i64) {
    // `User.confirmed.joins(:account).merge(Account.without_suspended).count`.
    let user_count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!" FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.confirmed_at IS NOT NULL AND a.suspended_at IS NULL"#,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // `Account.local.joins(:account_stat).sum('account_stats.statuses_count')`.
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

    // `Instance.count`: the `instances` view, which the scheduler refreshes,
    // blocked and allowed domains included.
    let domain_count = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!" FROM instances"#)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

    (user_count, status_count, domain_count)
}

/// `InstancePresenter::ContactPresenter#account`: the account
/// `site_contact_username` names (`Account.find_remote(username, domain)`,
/// the local domain read as none), or none while it is blank.
async fn fetch_contact_account(
    state: &AppState,
    settings: &crate::settings::Snapshot,
) -> Option<super::types::Account> {
    let configured = settings.string("site_contact_username");
    let handle = configured.trim();
    let handle = handle.strip_prefix('@').unwrap_or(handle);
    let (username, domain) = match handle.split_once('@') {
        Some((username, domain)) => (username, Some(domain)),
        None => (handle, None),
    };
    if username.is_empty() {
        return None;
    }
    let domain = domain.filter(|d| !crate::search::is_local_domain(state, d));
    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts
         WHERE lower(username) = lower($1)
           AND (($2::text IS NULL AND domain IS NULL) OR lower(domain) = lower($2))
         LIMIT 1",
        username,
        domain,
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
