//! Forgotten passwords: Devise's recoverable module as Mastodon's
//! `Auth::PasswordsController` serves it. Asking sends a link, without
//! saying whether the address has an account (`config.paranoid`); following
//! it within six hours sets a new password, ends every session and
//! authorization, and mails `password_change`. The person is not signed in
//! afterwards (`sign_in_after_reset_password = false`).

use axum::{
    extract::{Form, Query},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;

use crate::{locale::Locale, state::AppState};

const SENT: &str = "If your email address exists in our database, you will receive a password \
                    recovery link at your email address in a few minutes. Please check your spam \
                    folder if you didn't receive this email.";
const UPDATED: &str = "Your password has been changed successfully.";
const INVALID_TOKEN: &str = "Password reset token is invalid or expired. Please request a new one.";

fn locale(headers: &HeaderMap) -> Locale {
    Locale::detect(
        None,
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
    )
}

/// Whether the request came from a browser's form rather than an API client.
fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

fn render(
    state: &AppState,
    locale: Locale,
    mode: &str,
    token: &str,
    notice: Option<&str>,
    error: Option<&str>,
) -> Response {
    let html = crate::templates::render(
        "password_reset.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain => &state.instance.domain,
            mode,
            token,
            notice,
            error,
            t_reset_password => locale.t("reset_password"),
            t_set_new_password => locale.t("set_new_password"),
            t_email => locale.t("email"),
            t_new_password => locale.t("new_password"),
            t_confirm_new_password => locale.t("confirm_new_password"),
            t_sign_in => locale.t("sign_in"),
        },
    );
    Html(html).into_response()
}

#[derive(Debug, Default, Deserialize)]
pub struct NewQuery {
    /// Set when a link that no longer works sent the person here.
    pub invalid: Option<String>,
}

/// GET /auth/password/new
pub async fn new_page(
    state: AppState,
    headers: HeaderMap,
    Query(query): Query<NewQuery>,
) -> Response {
    let error = query.invalid.is_some().then_some(INVALID_TOKEN);
    render(&state, locale(&headers), "new", "", None, error)
}

#[derive(Debug, Deserialize)]
pub struct RequestForm {
    pub email: Option<String>,
}

/// POST /auth/password: `Devise::PasswordsController#create`. The answer is
/// the same whether or not the address has an account.
pub async fn request(
    state: AppState,
    headers: HeaderMap,
    Form(form): Form<RequestForm>,
) -> Response {
    let email = form.email.as_deref().map(str::trim).unwrap_or("");
    if !email.is_empty() {
        let user_id = sqlx::query_scalar!(
            "SELECT id FROM users WHERE lower(email) = lower($1) AND confirmed_at IS NOT NULL",
            email,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        if let Some(user_id) = user_id {
            if let Err(error) =
                crate::accounts::send_reset_password_instructions(&state, user_id).await
            {
                tracing::error!(%error, "could not send password reset instructions");
            }
        }
    }
    if wants_html(&headers) {
        return render(&state, locale(&headers), "sent", "", Some(SENT), None);
    }
    StatusCode::OK.into_response()
}

#[derive(Debug, Deserialize)]
pub struct EditQuery {
    pub reset_password_token: Option<String>,
}

/// GET /auth/password/edit: the new password form, for a token still good
/// (`redirect_invalid_reset_token`).
pub async fn edit_page(
    state: AppState,
    headers: HeaderMap,
    Query(query): Query<EditQuery>,
) -> Response {
    let token = query.reset_password_token.unwrap_or_default();
    if crate::accounts::reset_password_user(&state, &token)
        .await
        .is_err()
    {
        return Redirect::to("/auth/password/new?invalid=1").into_response();
    }
    render(&state, locale(&headers), "edit", &token, None, None)
}

#[derive(Debug, Deserialize)]
pub struct ResetForm {
    pub reset_password_token: Option<String>,
    /// The name eunha's first reset endpoint took the token under.
    pub token: Option<String>,
    pub password: Option<String>,
    pub password_confirmation: Option<String>,
}

/// POST /auth/password/edit (the form) and PUT /auth/password:
/// `Auth::PasswordsController#update`.
pub async fn update(
    state: AppState,
    headers: HeaderMap,
    crate::api::mastodon::extractors::FormOrJson(form): crate::api::mastodon::extractors::FormOrJson<
        ResetForm,
    >,
) -> Response {
    let token = form.reset_password_token.or(form.token).unwrap_or_default();
    let result = crate::accounts::reset_password_by_token(
        &state,
        &token,
        form.password.as_deref().unwrap_or(""),
        form.password_confirmation.as_deref(),
    )
    .await;
    let html = wants_html(&headers);
    match result {
        Ok(_) if html => render(&state, locale(&headers), "done", "", Some(UPDATED), None),
        Ok(_) => StatusCode::OK.into_response(),
        Err(message) if html => render(
            &state,
            locale(&headers),
            "edit",
            &token,
            None,
            Some(message),
        ),
        Err(message) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({ "error": message })),
        )
            .into_response(),
    }
}
