use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    /// A 404 that says why, as Mastodon's `quoted_status_not_found` does.
    #[error("not found: {0}")]
    NotFoundMsg(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("unauthorized: {0}")]
    UnauthorizedMsg(String),
    #[error("forbidden")]
    Forbidden,
    /// The token is valid but was not granted the scope this endpoint needs.
    ///
    /// Distinct from [`AppError::Forbidden`] because Mastodon says which it is,
    /// and the two call for different fixes: one is a permission the account
    /// lacks, the other an authorisation the app never asked for.
    #[error("outside the authorized scopes")]
    ForbiddenScope,
    /// A 403 that says why, as `require_user!` and its kin do.
    #[error("forbidden: {0}")]
    ForbiddenMsg(String),
    #[error("unprocessable entity: {0}")]
    Unprocessable(String),
    /// `ActiveRecord::RecordInvalid` as `ValidationErrorFormatter` renders
    /// it: a 422 with this body, `error` and `details`.
    #[error("invalid record: {0}")]
    Invalid(serde_json::Value),
    /// `Mastodon::InvalidParameterError` and `ActionController::ParameterMissing`.
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("conflict")]
    Conflict,
    /// `Mastodon::RateLimitExceededError`, with `I18n.t('errors.429')` in
    /// the instance's default locale.
    #[error("too many requests: {0}")]
    TooManyRequests(String),
    #[error("gone")]
    Gone(String),
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
    /// An exception Mastodon raises and nothing rescues, carrying its message.
    ///
    /// Rails' `ActionDispatch::PublicExceptions` answers it, with the body it
    /// renders for a JSON request; the message only reaches the log, as it
    /// only reaches upstream's.
    #[error("unrescued exception: {0}")]
    Unrescued(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "Record not found".to_string()),
            AppError::NotFoundMsg(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".to_string()),
            AppError::UnauthorizedMsg(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::Forbidden => (
                StatusCode::FORBIDDEN,
                "This action is not allowed".to_string(),
            ),
            AppError::ForbiddenScope => (
                StatusCode::FORBIDDEN,
                "This action is outside the authorized scopes".to_string(),
            ),
            AppError::ForbiddenMsg(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Unprocessable(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg.clone()),
            AppError::Invalid(body) => {
                return (StatusCode::UNPROCESSABLE_ENTITY, Json(body.clone())).into_response();
            }
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Conflict => (StatusCode::CONFLICT, "Duplicate record".to_string()),
            AppError::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
            AppError::Gone(msg) => (StatusCode::GONE, msg.clone()),
            AppError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
            AppError::Database(e) => {
                tracing::error!("database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database error".to_string(),
                )
            }
            AppError::Internal(e) => {
                tracing::error!("internal error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
            AppError::Unrescued(e) => {
                tracing::error!("unrescued exception: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "status": 500, "error": "Internal Server Error" })),
                )
                    .into_response();
            }
        };

        (status, Json(json!({ "error": message }))).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;

/// Longest error text any queue keeps. Enough to diagnose a failure without
/// letting a remote server write unbounded data into our tables.
const MAX_ERROR_TEXT: usize = 2000;

/// Make an error message safe to store in a Postgres `text` column.
///
/// Error strings on the queue tables are derived from remote input — a peer's
/// response body can end up inside a transport error — and Postgres `text`
/// rejects NUL bytes outright. An unsanitised NUL makes the UPDATE that records
/// a job's outcome fail, which leaves `attempts` unincremented and the lock
/// held: the job is then reclaimed when its lock goes stale and retried
/// forever, never reaching `max_attempts`. Stripping NULs is what keeps a
/// failing job able to fail *permanently*.
pub fn sanitize_error_text(error: &str) -> String {
    let cleaned: String = error.replace('\0', "");
    match cleaned.char_indices().nth(MAX_ERROR_TEXT) {
        // Truncate on a char boundary so multi-byte text can't be split.
        Some((idx, _)) => format!("{}…", &cleaned[..idx]),
        None => cleaned,
    }
}

#[cfg(test)]
mod error_text_tests {
    use super::{sanitize_error_text, MAX_ERROR_TEXT};

    #[test]
    fn strips_nul_bytes() {
        // Exactly the shape that wedged a delivery job: a NUL inside the text
        // of a transport error carrying part of a peer's response.
        let raw = "error sending request\0 for url (https://example.invalid/inbox)";
        let safe = sanitize_error_text(raw);
        assert!(!safe.contains('\0'), "NUL bytes must not survive");
        assert!(safe.starts_with("error sending request for url"));
    }

    #[test]
    fn caps_length_on_a_char_boundary() {
        let raw = "가".repeat(MAX_ERROR_TEXT * 2);
        let safe = sanitize_error_text(&raw);
        // Truncating mid-character would have panicked on the slice above.
        assert!(safe.chars().count() <= MAX_ERROR_TEXT + 1);
        assert!(safe.ends_with('…'));
    }

    #[test]
    fn leaves_ordinary_messages_alone() {
        let raw = "HTTP 502 from https://example.invalid/inbox";
        assert_eq!(sanitize_error_text(raw), raw);
    }
}
