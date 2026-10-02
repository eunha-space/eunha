//! The preferences Mastodon keeps only on its web settings pages, for an
//! account's own settings page: `Settings::PrivacyController`'s
//! "include profile page in search engines" (`noindex`, which the form shows
//! inverted as `indexable`) and "display from which app you sent a post"
//! (`show_application`), and from `Settings::Preferences::*` the interface
//! language, the languages to show in public timelines, and the staff
//! notification emails eunha sends.
//!
//! What Mastodon offers through the REST API already — the posting defaults
//! (`source[privacy]`, `source[sensitive]`, `source[language]`,
//! `source[quote_policy]`), `discoverable`, `indexable`, `locked`, `bot` and
//! `hide_collections` — stays with `PATCH /api/v1/accounts/update_credentials`.

use axum::{routing::get, Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::{
    accounts::user_setting_bool,
    api::{eunha::two_factor::signed_in, mastodon::extractors::Params},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

pub fn routes() -> Router {
    Router::new().route("/api/eunha/v1/preferences", get(show).patch(update))
}

/// The `notification_emails.*` settings for the mails eunha sends staff.
const NOTIFICATION_EMAILS: &[(&str, bool)] = &[
    ("report", true),
    ("pending_account", true),
    ("trends", true),
    ("appeal", true),
    ("end_of_support", true),
];

#[derive(Debug, Serialize)]
pub struct Preferences {
    /// Ask search engines not to index the profile (`noindex`). Defaults to
    /// `Setting.noindex`.
    pub noindex: bool,
    /// Show the app a post was sent from (`show_application`).
    pub show_application: bool,
    /// `users.chosen_languages`: only posts in these languages in public
    /// timelines, or every language when null.
    pub chosen_languages: Option<Vec<String>>,
    /// `users.locale`, the language mail is written in.
    pub locale: Option<String>,
    pub notification_emails: NotificationEmails,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct NotificationEmails {
    pub report: Option<bool>,
    pub pending_account: Option<bool>,
    pub trends: Option<bool>,
    pub appeal: Option<bool>,
    pub end_of_support: Option<bool>,
    /// `none`, `critical`, `patch` or `all`.
    pub software_updates: Option<String>,
}

async fn load(state: &AppState, user_id: i64) -> AppResult<Preferences> {
    let row = sqlx::query!(
        "SELECT settings, chosen_languages, locale FROM users WHERE id = $1",
        user_id,
    )
    .fetch_one(&state.db)
    .await?;
    let settings = row.settings.as_deref();
    let site_noindex = crate::settings::boolean(state, "noindex").await;
    let bool_of = |key: &str, default: bool| Some(user_setting_bool(settings, key, default));
    let software_updates = settings
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| {
            v.get("notification_emails.software_updates")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "critical".into());
    Ok(Preferences {
        noindex: user_setting_bool(settings, "noindex", site_noindex),
        show_application: user_setting_bool(settings, "show_application", true),
        chosen_languages: row.chosen_languages.filter(|l| !l.is_empty()),
        locale: row.locale,
        notification_emails: NotificationEmails {
            report: bool_of("notification_emails.report", true),
            pending_account: bool_of("notification_emails.pending_account", true),
            trends: bool_of("notification_emails.trends", true),
            appeal: bool_of("notification_emails.appeal", true),
            end_of_support: bool_of("notification_emails.end_of_support", true),
            software_updates: Some(software_updates),
        },
    })
}

/// GET /api/eunha/v1/preferences
pub async fn show(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Preferences>> {
    let (_, user_id) = signed_in(auth, "read:accounts")?;
    Ok(Json(load(&state, user_id).await?))
}

#[derive(Debug, Deserialize, Default)]
pub struct Update {
    pub noindex: Option<bool>,
    /// What Mastodon's privacy form posts: `noindex`, inverted.
    pub indexable: Option<bool>,
    pub show_application: Option<bool>,
    /// An empty list clears it.
    pub chosen_languages: Option<Vec<String>>,
    pub locale: Option<String>,
    #[serde(default)]
    pub notification_emails: NotificationEmails,
}

/// PATCH /api/eunha/v1/preferences
///
/// `Settings::PrivacyController#update` and
/// `Settings::Preferences::BaseController#update`, for the keys above. Only
/// what the request names changes.
pub async fn update(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Update>,
) -> AppResult<Json<Preferences>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;

    let raw = sqlx::query_scalar!("SELECT settings FROM users WHERE id = $1", user_id)
        .fetch_one(&state.db)
        .await?;
    let mut settings = raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let object = settings.as_object_mut().expect("an object");
    let mut changed = false;
    let mut set = |key: &str, value: serde_json::Value| {
        object.insert(key.to_owned(), value);
        changed = true;
    };

    // `setting_inverse_alias :indexable, :noindex`.
    if let Some(noindex) = form.noindex.or(form.indexable.map(|i| !i)) {
        set("noindex", noindex.into());
    }
    if let Some(show) = form.show_application {
        set("show_application", show.into());
    }
    let emails = &form.notification_emails;
    for ((key, _), value) in NOTIFICATION_EMAILS.iter().zip([
        emails.report,
        emails.pending_account,
        emails.trends,
        emails.appeal,
        emails.end_of_support,
    ]) {
        if let Some(value) = value {
            set(&format!("notification_emails.{key}"), value.into());
        }
    }
    if let Some(level) = &emails.software_updates {
        // `in: %w(none critical patch all)`.
        if !matches!(level.as_str(), "none" | "critical" | "patch" | "all") {
            return Err(AppError::Unprocessable(
                "Validation failed: Software updates is not included in the list".into(),
            ));
        }
        set("notification_emails.software_updates", level.clone().into());
    }
    if changed {
        sqlx::query!(
            "UPDATE users SET settings = $1, updated_at = now() WHERE id = $2",
            settings.to_string(),
            user_id,
        )
        .execute(&state.db)
        .await?;
    }

    if let Some(languages) = form.chosen_languages {
        // `normalizes :chosen_languages, with: compact_blank.presence`, of
        // the languages the form offers.
        let chosen: Vec<String> = languages
            .into_iter()
            .map(|l| l.trim().to_owned())
            .filter(|l| crate::languages::valid_locale(Some(l)))
            .collect();
        let chosen = (!chosen.is_empty()).then_some(chosen);
        sqlx::query!(
            "UPDATE users SET chosen_languages = $1, updated_at = now() WHERE id = $2",
            chosen.as_deref(),
            user_id,
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(locale) = form.locale {
        // `normalizes :locale`: one Mastodon cannot speak is no locale.
        let locale = locale.trim();
        let known = crate::languages::valid_locale(Some(locale))
            || locale
                .split_once('-')
                .is_some_and(|(language, _)| crate::languages::valid_locale(Some(language)));
        sqlx::query!(
            "UPDATE users SET locale = $1, updated_at = now() WHERE id = $2",
            known.then_some(locale),
            user_id,
        )
        .execute(&state.db)
        .await?;
    }

    Ok(Json(load(&state, user_id).await?))
}
