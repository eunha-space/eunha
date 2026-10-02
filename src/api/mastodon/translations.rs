//! `Api::V1::Statuses::TranslationsController` and
//! `Api::V1::Instances::TranslationLanguagesController`.

use axum::{
    extract::{Extension, Path},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::extractors::Params;
use crate::{
    db::models::{vis, Status as DbStatus},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
    translation::{self, Source, StatusSource, TranslateError},
};

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// `respond_with_error(404)`, which says `Not Found` rather than the
/// `Record not found` of a missing record elsewhere.
fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "Not Found")
}

// ── GET /api/v1/instance/translation_languages ───────────────────────────

/// The configured service's language pairs, `{}` when there is none.
pub async fn get_translation_languages(state: AppState) -> Response {
    if !state.instance.translation.configured() {
        return Json(serde_json::json!({})).into_response();
    }
    match translation::languages(&state).await {
        Ok(languages) => Json(languages.to_json()).into_response(),
        // `Api::ErrorHandling` turns a connection failure into a 503; the
        // service's own errors are not rescued here, and are a 500.
        Err(translation::Error::Connection(e)) => {
            tracing::warn!(error = %e, "could not list translation languages");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Remote data could not be fetched",
            )
        }
        Err(e) => AppError::Internal(anyhow::anyhow!("listing translation languages: {e}"))
            .into_response(),
    }
}

// ── POST /api/v1/statuses/:id/translate ───────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct TranslateParams {
    /// `Localized`'s `params[:lang]`, which picks the locale and so the
    /// target language.
    #[serde(default)]
    lang: Option<serde_json::Value>,
}

/// `Localized#requested_locale`: the `lang` parameter, the user's locale, the
/// `Accept-Language` header, then `I18n.default_locale`.
async fn requested_locale(
    state: &AppState,
    auth: &AuthenticatedUser,
    headers: &HeaderMap,
    lang: Option<&str>,
) -> AppResult<String> {
    if let Some(locale) = lang.and_then(crate::languages::available_locale) {
        return Ok(locale.to_string());
    }
    if let Some(user_id) = auth.user_id {
        let locale = sqlx::query_scalar!("SELECT locale FROM users WHERE id = $1", user_id)
            .fetch_optional(&state.db)
            .await?
            .flatten();
        if let Some(locale) = locale
            .as_deref()
            .and_then(crate::languages::available_locale)
        {
            return Ok(locale.to_string());
        }
    }
    if let Some(locale) = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::languages::accept_language_locale)
    {
        return Ok(locale.to_string());
    }
    Ok(super::DEFAULT_LOCALE.to_string())
}

/// `Api::V1::Statuses::BaseController#set_status`: `Status.find` and
/// `authorize @status, :show?`, either failing as a 404.
async fn visible_status(
    state: &AppState,
    id: i64,
    viewer_id: i64,
) -> Result<Option<DbStatus>, AppError> {
    let Some(status) = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };
    if viewer_id != status.account_id {
        let blocked = sqlx::query_scalar!(
            r#"SELECT 1 FROM blocks
               WHERE (account_id = $1 AND target_account_id = $2)
                  OR (account_id = $2 AND target_account_id = $1)"#,
            viewer_id,
            status.account_id
        )
        .fetch_optional(&state.db)
        .await?
        .is_some();
        if blocked {
            return Ok(None);
        }
    }
    match super::statuses::check_status_visible(state, &status, viewer_id).await {
        Ok(()) => Ok(Some(status)),
        Err(AppError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `String#present?`.
fn present(text: &str) -> bool {
    !text.trim().is_empty()
}

pub async fn translate_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Params(params): Params<TranslateParams>,
) -> AppResult<Response> {
    auth.require_scope("read:statuses")?;
    let Some(status) = visible_status(&state, id, auth.account_id).await? else {
        return Ok(not_found());
    };
    let api = super::statuses::serialize_status(&state, &status, Some(auth.account_id)).await?;

    // `TranslateStatusService#source_texts`.
    let shortcodes: Vec<String> = api.emojis.iter().map(|e| e.shortcode.clone()).collect();
    let wrap = |html: &str| translation::wrap_emoji_shortcodes(html, &shortcodes);
    let mut texts = Vec::new();
    if present(&status.text) {
        texts.push((Source::Content, wrap(&api.content)));
    }
    if present(&status.spoiler_text) {
        texts.push((
            Source::SpoilerText,
            wrap(&translation::html_escape(&status.spoiler_text)),
        ));
    }
    if let Some(poll) = &api.poll {
        for (index, option) in poll.options.iter().enumerate() {
            texts.push((
                Source::PollOption(index),
                wrap(&translation::html_escape(&option.title)),
            ));
        }
    }
    for media in &api.media_attachments {
        texts.push((
            Source::MediaAttachment(media.id.clone()),
            translation::html_escape(media.description.as_deref().unwrap_or("")),
        ));
    }
    let source = StatusSource {
        language: status.language.clone(),
        texts,
    };

    let lang = params.lang.as_ref().and_then(|v| v.as_str());
    let target = requested_locale(&state, &auth, &headers, lang).await?;
    let distributable = matches!(status.visibility, vis::PUBLIC | vis::UNLISTED);

    let translated =
        match translation::translate_status(&state, &source, distributable, &target).await {
            Ok(t) => t,
            Err(e) => return Ok(translate_error(e)),
        };

    // `REST::TranslationSerializer`.
    let poll = api.poll.as_ref().map(|poll| {
        serde_json::json!({
            "id": poll.id,
            "options": translated
                .poll_options
                .iter()
                .map(|title| serde_json::json!({ "title": title }))
                .collect::<Vec<_>>(),
        })
    });
    let media_attachments: Vec<_> = translated
        .media_attachments
        .iter()
        .map(|(id, description)| serde_json::json!({ "id": id, "description": description }))
        .collect();
    Ok(Json(serde_json::json!({
        "detected_source_language": translated.detected_source_language,
        "language": translated.language,
        "provider": translated.provider,
        "spoiler_text": translated.spoiler_text,
        "content": translated.content,
        "poll": poll,
        "media_attachments": media_attachments,
    }))
    .into_response())
}

/// The controller's `rescue_from`s, then `Api::ErrorHandling`'s.
fn translate_error(e: TranslateError) -> Response {
    use translation::Error;
    match e {
        TranslateError::NotPermitted => AppError::Forbidden.into_response(),
        TranslateError::Service(Error::NotConfigured) => not_found(),
        TranslateError::Service(Error::QuotaExceeded) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The server-wide usage quota for the translation service has been exceeded.",
        ),
        TranslateError::Service(Error::TooManyRequests) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "There have been too many requests to the translation service recently.",
        ),
        TranslateError::Service(Error::UnexpectedResponse) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable")
        }
        TranslateError::Service(Error::Connection(e)) => {
            tracing::warn!(error = %e, "could not reach the translation service");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Remote data could not be fetched",
            )
        }
        TranslateError::Service(e @ Error::MalformedLanguages) => {
            AppError::Internal(anyhow::anyhow!("{e}")).into_response()
        }
    }
}
