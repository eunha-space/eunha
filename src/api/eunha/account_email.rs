//! A member changing their own email address: the email half of Mastodon's
//! account settings form (`Auth::RegistrationsController#update`, Devise's
//! `update_with_password` with `reconfirmable`), which Mastodon has only as a
//! web form behind a session.
//!
//! The current password is the challenge. A different address goes through
//! `User`'s validations, then waits in `unconfirmed_email` until the link
//! mailed to it (`reconfirmation_instructions`) is followed, and the address
//! being left is told (`email_changed`), as `send_email_changed_notification`
//! does with `reconfirmable`.

use axum::{
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    api::{eunha::two_factor::signed_in, mastodon::extractors::Params},
    email_subscriptions::ValidationErrors,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

pub fn routes() -> Router {
    Router::new().route("/api/eunha/v1/email", get(show).put(update))
}

#[derive(Debug, Serialize)]
pub struct Email {
    /// The confirmed address mail goes to.
    pub email: String,
    /// The address waiting for its link to be followed, if any.
    pub unconfirmed_email: Option<String>,
}

async fn load(state: &AppState, user_id: i64) -> AppResult<Email> {
    let row = sqlx::query!(
        "SELECT email, unconfirmed_email FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Email {
        email: row.email,
        unconfirmed_email: row.unconfirmed_email.filter(|e| !e.is_empty()),
    })
}

/// GET /api/eunha/v1/email
pub async fn show(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Email>> {
    let (_, user_id) = signed_in(auth, "read:accounts")?;
    Ok(Json(load(&state, user_id).await?))
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub email: Option<String>,
    pub current_password: Option<String>,
}

/// `User`'s `email` `length: { maximum: 320 }`.
const EMAIL_MAXIMUM: usize = 320;

/// PUT /api/eunha/v1/email
pub async fn update(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Update>,
) -> AppResult<Response> {
    let (account_id, user_id) = signed_in(auth, "write:accounts")?;
    // `require_not_suspended!`
    let unavailable = sqlx::query_scalar!(
        r#"SELECT (suspended_at IS NOT NULL OR requested_deletion_at IS NOT NULL) AS "u!"
           FROM accounts WHERE id = $1"#,
        account_id
    )
    .fetch_one(&state.db)
    .await?;
    if unavailable {
        return Err(AppError::Forbidden);
    }
    let user = sqlx::query!(
        r#"SELECT email, encrypted_password, host(sign_up_ip) AS sign_up_ip,
                  (confirmed_at IS NOT NULL) AS "confirmed!"
           FROM users WHERE id = $1"#,
        user_id
    )
    .fetch_one(&state.db)
    .await?;

    // Devise's `strip_whitespace_keys` and `case_insensitive_keys`.
    let new_email = form.email.as_deref().unwrap_or("").trim().to_lowercase();
    let changing = new_email != user.email;
    let mut errors = ValidationErrors::default();
    if changing {
        validate(
            &state,
            user_id,
            &new_email,
            user.confirmed,
            user.sign_up_ip.as_deref().and_then(|ip| ip.parse().ok()),
            &mut errors,
        )
        .await;
    }
    // `update_with_password`: the current password, whatever else is wrong.
    let password = form.current_password.as_deref().unwrap_or("");
    if password.is_empty() {
        errors.add("current_password", "blank", "can't be blank");
    } else if user.encrypted_password.is_empty()
        || crate::crypto::verify_password(password, &user.encrypted_password)
            .await
            .is_err()
    {
        errors.add("current_password", "invalid", "is invalid");
    }
    if !errors.is_empty() {
        return Ok(errors.into_response());
    }

    if changing {
        crate::accounts::set_unconfirmed_email(&state.db, user_id, &new_email).await?;
        // `send_reconfirmation_instructions`
        crate::accounts::send_confirmation_instructions(&state, user_id).await?;
        // `send_email_changed_notification`, to the address being left.
        if let Some(two_factor) = crate::two_factor::load(&state.db, user_id).await? {
            crate::two_factor::notify(
                &state,
                &two_factor,
                crate::two_factor::NoticeKind::Security(
                    crate::two_factor::OwnedNotice::EmailChanged(new_email.clone()),
                ),
            );
        }
    }
    Ok(Json(load(&state, user_id).await?).into_response())
}

/// The validations `User` runs on a changed `email`: presence, Mastodon's
/// `EmailAddressValidator`, the length, Devise's uniqueness,
/// `EmailMxValidator` (the address's domain must take mail and be neither
/// blocked nor served by a blocked mail host), and `UserEmailValidator`, which
/// for a confirmed user applies only when the email domain lists are set to
/// apply after confirmation, as they are not here.
async fn validate(
    state: &AppState,
    user_id: i64,
    email: &str,
    confirmed: bool,
    sign_up_ip: Option<std::net::IpAddr>,
    errors: &mut ValidationErrors,
) {
    use crate::moderation::signup;
    if email.is_empty() {
        errors.add("email", "blank", "can't be blank");
        return;
    }
    if !crate::accounts::valid_email(email) {
        errors.add("email", "invalid", "is invalid");
    }
    if email.chars().count() > EMAIL_MAXIMUM {
        errors.add(
            "email",
            "too_long",
            "is too long (maximum is 320 characters)",
        );
    }
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = $1 AND id <> $2) AS "e!""#,
        email,
        user_id
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);
    if taken {
        errors.add("email", "taken", "has already been taken");
    }
    if !signup::mx_check_skipped() {
        match signup::email_domain(email) {
            None => errors.add("email", "invalid", "is invalid"),
            Some(domain) => mx_validate(state, domain, sign_up_ip, errors).await,
        }
    }
    if !confirmed {
        let address = [email.to_owned()];
        if signup::email_domain_blocked(state, &address, false, sign_up_ip).await {
            errors.add("email", "blocked", "is using a disallowed e-mail provider");
        }
        if signup::canonical_email_blocked(state, email).await {
            errors.add("email", "taken", "has already been taken");
        }
    }
}

/// `EmailMxValidator`, for a domain that could be read.
async fn mx_validate(
    state: &AppState,
    domain: String,
    sign_up_ip: Option<std::net::IpAddr>,
    errors: &mut ValidationErrors,
) {
    use crate::moderation::signup;
    let mx = signup::resolve_mx(&domain).await;
    if mx.ips.is_empty() {
        errors.add("email", "unreachable", "does not seem to exist");
        return;
    }
    let mut domains = vec![domain];
    domains.extend(mx.records);
    if signup::email_domain_blocked(state, &domains, false, sign_up_ip).await {
        errors.add("email", "blocked", "is using a disallowed e-mail provider");
    }
}
