//! `Admin::SettingsController` and its pages (branding, about, registrations,
//! discovery, content retention, appearance), over `Form::AdminSettings`, and
//! `Admin::SiteUploadsController`.

use std::collections::HashMap;

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde_json::{json, Map, Value};
use serde_yaml::Value as Yaml;

use super::super::extractors::{Part, Parts};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::flag,
    settings::Snapshot,
    site_uploads,
    state::AppState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Boolean,
    Integer,
    Upload,
}

/// `Form::AdminSettings::KEYS`, each with how `typecast_value` reads it.
const KEYS: &[(&str, Kind)] = &[
    ("site_contact_username", Kind::Text),
    ("site_contact_email", Kind::Text),
    ("site_title", Kind::Text),
    ("site_short_description", Kind::Text),
    ("site_extended_description", Kind::Text),
    ("site_terms", Kind::Text),
    ("registrations_mode", Kind::Text),
    ("closed_registrations_message", Kind::Text),
    ("bootstrap_timeline_accounts", Kind::Text),
    ("theme", Kind::Text),
    ("activity_api_enabled", Kind::Boolean),
    ("peers_api_enabled", Kind::Boolean),
    ("preview_sensitive_media", Kind::Boolean),
    ("custom_css", Kind::Text),
    ("profile_directory", Kind::Boolean),
    ("thumbnail", Kind::Upload),
    ("thumbnail_description", Kind::Text),
    ("mascot", Kind::Upload),
    ("trends", Kind::Boolean),
    ("trendable_by_default", Kind::Boolean),
    ("show_domain_blocks", Kind::Text),
    ("show_domain_blocks_rationale", Kind::Text),
    ("allow_referrer_origin", Kind::Boolean),
    ("noindex", Kind::Boolean),
    ("require_invite_text", Kind::Boolean),
    ("media_cache_retention_period", Kind::Integer),
    ("content_cache_retention_period", Kind::Integer),
    ("backups_retention_period", Kind::Integer),
    ("status_page_url", Kind::Text),
    ("captcha_enabled", Kind::Boolean),
    ("authorized_fetch", Kind::Boolean),
    ("app_icon", Kind::Upload),
    ("favicon", Kind::Upload),
    ("min_age", Kind::Integer),
    ("local_live_feed_access", Kind::Text),
    ("remote_live_feed_access", Kind::Text),
    ("local_topic_feed_access", Kind::Text),
    ("remote_topic_feed_access", Kind::Text),
    ("landing_page", Kind::Text),
    ("wrapstodon", Kind::Boolean),
    ("email_footer_text", Kind::Text),
];

const DESCRIPTION_LIMIT: usize = 200;
const DOMAIN_BLOCK_AUDIENCES: &[&str] = &["disabled", "users", "all"];
const REGISTRATION_MODES: &[&str] = &["open", "approved", "none"];
const FEED_ACCESS_MODES: &[&str] = &["public", "authenticated", "disabled"];
const ALTERNATE_FEED_ACCESS_MODES: &[&str] = &["public", "authenticated"];
const LANDING_PAGE: &[&str] = &["trends", "overview", "local_feed", "about"];

/// `SettingsPolicy`: every action is `manage_settings`.
async fn authorize(state: &AppState, auth: &AuthenticatedUser, scope: &str) -> AppResult<()> {
    auth.require_scope(scope)?;
    super::require_permission(state, auth.account_id, flag::MANAGE_SETTINGS).await
}

/// What the form shows for each key: the saved value, or the default, which
/// for the keys the instance configuration carries is the configured one.
async fn current(state: &AppState) -> AppResult<Value> {
    let snapshot = Snapshot::load(state).await;
    let instance = &state.instance;
    let uploads: HashMap<String, site_uploads::SiteUpload> = site_uploads::all(state)
        .await?
        .into_iter()
        .map(|u| (u.var.clone(), u))
        .collect();
    let mut out = Map::new();
    for &(key, kind) in KEYS {
        let value = match (key, kind) {
            ("site_title", _) => json!(snapshot.site_title(instance)),
            ("site_short_description", _) => json!(snapshot.site_short_description(instance)),
            ("site_extended_description", _) => {
                json!(snapshot.site_extended_description(instance))
            }
            ("site_contact_email", _) => json!(snapshot.site_contact_email(instance)),
            ("registrations_mode", _) => json!(snapshot.registrations_mode(instance).as_str()),
            // `OVERRIDEN_SETTINGS`: what is in force, not what was saved.
            ("authorized_fetch", _) => json!(crate::settings::authorized_fetch_override(instance)
                .unwrap_or_else(|| snapshot.boolean("authorized_fetch"))),
            (_, Kind::Text) => json!(snapshot.string(key)),
            (_, Kind::Boolean) => json!(snapshot.boolean(key)),
            (_, Kind::Integer) => snapshot.integer(key).map_or(Value::Null, |n| json!(n)),
            (_, Kind::Upload) => uploads
                .get(key)
                .map_or(Value::Null, |u| json!(u.entity(state))),
        };
        out.insert(key.to_owned(), value);
    }
    // The keys the instance configuration decides whatever is saved, which
    // Mastodon's form shows disabled.
    let overridden: Vec<&str> = crate::settings::authorized_fetch_override(instance)
        .map(|_| "authorized_fetch")
        .into_iter()
        .collect();
    out.insert("overridden".into(), json!(overridden));
    Ok(Value::Object(out))
}

/// `GET /api/v1/admin/settings`: `Settings#show`, every page's keys at once.
pub async fn get_admin_settings(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Value>> {
    authorize(&state, &auth, "admin:read").await?;
    Ok(Json(current(&state).await?))
}

/// `typecast_value`: a boolean is `'1'`, which a JSON client sends as `true`.
fn cast_boolean(part: &Part) -> bool {
    matches!(
        part.text().trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "t" | "on"
    )
}

/// Rails' `humanize` of an attribute name.
fn humanize(key: &str) -> String {
    let text = key.replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// `ExistingUsernameValidator`: the `username` or `username@domain` entries,
/// comma-separated, that name no known account.
async fn usernames_with_no_accounts(
    state: &AppState,
    value: &str,
) -> AppResult<(Vec<String>, usize)> {
    let mut missing = vec![];
    let mut count = 0;
    for entry in value.split(',') {
        let stripped = entry.trim().trim_start_matches('@');
        let (username, domain) = match stripped.split_once('@') {
            Some((u, d)) => (u, Some(d)),
            None => (stripped, None),
        };
        if username.trim().is_empty() {
            continue;
        }
        count += 1;
        // `TagManager#local_domain?`.
        let domain = domain.filter(|d| {
            !d.eq_ignore_ascii_case(&state.instance.domain)
                && !state
                    .instance
                    .aliases
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(d))
        });
        let found = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM accounts
                 WHERE lower(username) = lower($1)
                   AND (($2::text IS NULL AND domain IS NULL) OR lower(domain) = lower($2))
               ) AS "e!""#,
            username,
            domain,
        )
        .fetch_one(&state.db)
        .await?;
        if !found {
            missing.push(entry.to_owned());
        }
    }
    Ok((missing, count))
}

/// `URLValidator`: an absolute http or https URL with a host.
fn compliant_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

/// `PATCH /api/v1/admin/settings`: `Settings#update`. Only the keys given are
/// validated and saved, as only the keys a page posts are; uploads come as
/// files in a multipart body.
pub async fn update_admin_settings(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Parts(parts): Parts,
) -> AppResult<Json<Value>> {
    authorize(&state, &auth, "admin:write").await?;

    // `params.expect(form_admin_settings: [...])`, or the bare keys.
    let mut given: HashMap<String, Part> = HashMap::new();
    for (name, part) in parts {
        let key = name
            .strip_prefix("form_admin_settings[")
            .and_then(|k| k.strip_suffix(']'))
            .unwrap_or(&name)
            .to_owned();
        if KEYS.iter().any(|(k, _)| *k == key) {
            given.insert(key, part);
        }
    }
    let text = |key: &str| given.get(key).map(|p| p.text());

    let snapshot = Snapshot::load(&state).await;
    let instance = &state.instance;
    let mut errors: Vec<String> = vec![];
    let mut error =
        |key: &str, message: String| errors.push(format!("{} {message}", humanize(key)));

    let inclusion = |key: &str, allowed: &[&str], error: &mut dyn FnMut(&str, String)| {
        if let Some(value) = given.get(key).map(|p| p.text()) {
            if !allowed.contains(&value.as_str()) {
                error(key, "is not included in the list".into());
            }
        }
    };

    inclusion("registrations_mode", REGISTRATION_MODES, &mut error);
    if given.contains_key("site_contact_username") || given.contains_key("site_contact_email") {
        let username = text("site_contact_username")
            .unwrap_or_else(|| snapshot.string("site_contact_username"));
        let email =
            text("site_contact_email").unwrap_or_else(|| snapshot.site_contact_email(instance));
        if email.trim().is_empty() {
            error("site_contact_email", "can't be blank".into());
        }
        if username.trim().is_empty() {
            error("site_contact_username", "can't be blank".into());
        }
    }
    if let Some(username) = text("site_contact_username").filter(|u| !u.trim().is_empty()) {
        let (missing, count) = usernames_with_no_accounts(&state, &username).await?;
        if !missing.is_empty() || count > 1 {
            error(
                "site_contact_username",
                "could not find a local user with that username".into(),
            );
        }
    }
    if let Some(accounts) = text("bootstrap_timeline_accounts").filter(|u| !u.trim().is_empty()) {
        let (missing, _) = usernames_with_no_accounts(&state, &accounts).await?;
        if !missing.is_empty() {
            error(
                "bootstrap_timeline_accounts",
                format!("could not find {}", missing.join(", ")),
            );
        }
    }
    inclusion("show_domain_blocks", DOMAIN_BLOCK_AUDIENCES, &mut error);
    inclusion(
        "show_domain_blocks_rationale",
        DOMAIN_BLOCK_AUDIENCES,
        &mut error,
    );
    inclusion("local_live_feed_access", FEED_ACCESS_MODES, &mut error);
    inclusion("remote_live_feed_access", FEED_ACCESS_MODES, &mut error);
    inclusion(
        "local_topic_feed_access",
        ALTERNATE_FEED_ACCESS_MODES,
        &mut error,
    );
    inclusion("remote_topic_feed_access", FEED_ACCESS_MODES, &mut error);
    // `numericality: { only_integer: true }, allow_blank: true`.
    let mut integers: HashMap<&str, Yaml> = HashMap::new();
    for &(key, kind) in KEYS {
        if kind != Kind::Integer {
            continue;
        }
        let Some(value) = text(key) else { continue };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            integers.insert(key, Yaml::from(value));
        } else if let Ok(n) = trimmed.parse::<i64>() {
            integers.insert(key, Yaml::from(n));
        } else if trimmed.parse::<f64>().is_ok() {
            error(key, "must be an integer".into());
        } else {
            error(key, "is not a number".into());
        }
    }
    for key in ["site_short_description", "thumbnail_description"] {
        if text(key).is_some_and(|v| v.chars().count() > DESCRIPTION_LIMIT) {
            error(
                key,
                format!("is too long (maximum is {DESCRIPTION_LIMIT} characters)"),
            );
        }
    }
    let status_page_url =
        text("status_page_url").unwrap_or_else(|| snapshot.string("status_page_url"));
    if !status_page_url.trim().is_empty() && !compliant_url(&status_page_url) {
        error("status_page_url", "is invalid".into());
    }
    // `validate_site_uploads`.
    let mut uploads: Vec<(&str, String, Vec<u8>)> = vec![];
    for var in site_uploads::VARS {
        let Some(part) = given.get(var) else { continue };
        let Part::File {
            content_type, data, ..
        } = part
        else {
            // A blank field leaves the upload as it is.
            continue;
        };
        if data.is_empty() {
            continue;
        }
        if site_uploads::validate_content_type(content_type).is_err()
            || image::load_from_memory(data).is_err()
        {
            error(var, "is invalid".into());
            continue;
        }
        uploads.push((var, content_type.clone(), data.clone()));
    }
    inclusion("landing_page", LANDING_PAGE, &mut error);

    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }

    let mut tx = state.db.begin().await?;
    for &(key, kind) in KEYS {
        let Some(part) = given.get(key) else { continue };
        let value = match kind {
            Kind::Upload => continue,
            Kind::Boolean => Yaml::from(cast_boolean(part)),
            Kind::Integer => integers.remove(key).unwrap_or(Yaml::Null),
            Kind::Text => Yaml::from(part.text()),
        };
        crate::settings::set_in(&mut *tx, key, value).await?;
    }
    tx.commit().await?;

    for (var, content_type, data) in uploads {
        match site_uploads::save(&state, var, &content_type, data).await {
            Ok(_) => {}
            Err(site_uploads::SaveError::Invalid(_)) => {
                return Err(AppError::Unprocessable(format!(
                    "Validation failed: {} is invalid",
                    humanize(var)
                )));
            }
            Err(site_uploads::SaveError::Internal(e)) => return Err(e),
        }
    }

    Ok(Json(current(&state).await?))
}

/// `DELETE /api/v1/admin/site_uploads/:id`: `SiteUploads#destroy`.
pub async fn delete_site_upload(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    authorize(&state, &auth, "admin:write").await?;
    site_uploads::destroy(&state, id).await?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    #[test]
    fn humanizes_like_rails() {
        assert_eq!(super::humanize("site_contact_email"), "Site contact email");
        assert_eq!(super::humanize("status_page_url"), "Status page url");
    }

    #[test]
    fn urls_need_a_web_scheme_and_a_host() {
        assert!(super::compliant_url("https://status.example.com"));
        assert!(!super::compliant_url("ftp://example.com"));
        assert!(!super::compliant_url("example.com"));
    }
}
