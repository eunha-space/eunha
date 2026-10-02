//! An account's own two-factor authentication: Mastodon's
//! `Settings::TwoFactorAuthenticationMethodsController`, the
//! `Settings::TwoFactorAuthentication::*` controllers, and its security keys.
//!
//! Mastodon has these only as web forms behind a session; eunha's settings
//! page is a single-page app holding a token, so they are served here. What
//! Mastodon guards with `ChallengableConcern` — starting setup, new recovery
//! codes, turning it off — asks for the password with the request, as do the
//! security key changes, which a session-less API would otherwise leave to
//! any token with `write:accounts`.

use axum::{
    extract::Path,
    routing::{delete, get, post},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    api::mastodon::extractors::Params,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
    two_factor::{self, NoticeKind, OwnedNotice, UserTwoFactor},
};

/// `ChallengableConcern::CHALLENGE_TIMEOUT`: how long a setup begun with the
/// password may wait for its first code.
const SETUP_TTL_SECONDS: u64 = 3600;

pub fn routes() -> Router {
    Router::new()
        .route(
            "/api/eunha/v1/two_factor_authentication",
            get(show).delete(disable),
        )
        .route(
            "/api/eunha/v1/two_factor_authentication/otp",
            post(begin_otp),
        )
        .route(
            "/api/eunha/v1/two_factor_authentication/otp/confirm",
            post(confirm_otp),
        )
        .route(
            "/api/eunha/v1/two_factor_authentication/recovery_codes",
            post(regenerate_recovery_codes),
        )
        .route(
            "/api/eunha/v1/two_factor_authentication/webauthn_credentials/options",
            post(webauthn_options),
        )
        .route(
            "/api/eunha/v1/two_factor_authentication/webauthn_credentials",
            post(add_webauthn_credential),
        )
        .route(
            "/api/eunha/v1/two_factor_authentication/webauthn_credentials/{id}",
            delete(remove_webauthn_credential),
        )
}

#[derive(Debug, Serialize)]
pub struct WebauthnCredential {
    pub id: String,
    pub nickname: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct Status {
    /// `User#otp_enabled?`.
    pub otp_enabled: bool,
    /// `User#webauthn_enabled?`.
    pub webauthn_enabled: bool,
    /// The role requires two-factor authentication (`UserRole#require_2fa`).
    pub required: bool,
    /// Recovery codes not yet used.
    pub recovery_codes_remaining: usize,
    pub webauthn_credentials: Vec<WebauthnCredential>,
    /// Whether TOTP can be set up here at all: the instance has its
    /// ActiveRecord encryption keys.
    pub available: bool,
}

/// A time as the API renders them.
pub fn timestamp(at: chrono::NaiveDateTime) -> String {
    at.and_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn signed_in(
    auth: Option<Extension<AuthenticatedUser>>,
    scope: &str,
) -> AppResult<(i64, i64)> {
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    auth.require_scope(scope)?;
    let user_id = auth.user_id.ok_or_else(|| {
        AppError::Unprocessable("This method requires an authenticated user".into())
    })?;
    Ok((auth.account_id, user_id))
}

async fn user(state: &AppState, user_id: i64) -> AppResult<UserTwoFactor> {
    two_factor::load(&state.db, user_id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn status(state: &AppState, user_id: i64) -> AppResult<Status> {
    let user = user(state, user_id).await?;
    let keys = sqlx::query!(
        "SELECT id, nickname, created_at FROM webauthn_credentials WHERE user_id = $1 ORDER BY id",
        user_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Status {
        otp_enabled: user.otp_enabled(),
        webauthn_enabled: user.webauthn_enabled(),
        required: user.role_requires,
        recovery_codes_remaining: if user.otp_enabled() {
            user.otp_backup_codes.len()
        } else {
            0
        },
        webauthn_credentials: keys
            .into_iter()
            .map(|k| WebauthnCredential {
                id: k.id.to_string(),
                nickname: k.nickname,
                created_at: timestamp(k.created_at),
            })
            .collect(),
        available: state.encryptor.is_some(),
    })
}

/// `ChallengableConcern#require_challenge!`: the password, unless the user
/// has none.
async fn require_challenge(
    state: &AppState,
    user_id: i64,
    password: Option<&str>,
) -> AppResult<()> {
    let hash = sqlx::query_scalar!(
        "SELECT encrypted_password FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&state.db)
    .await?;
    if hash.is_empty() {
        return Ok(());
    }
    crate::crypto::verify_password(password.unwrap_or(""), &hash)
        .await
        .map_err(|_| AppError::Unprocessable("Invalid password".into()))
}

/// GET /api/eunha/v1/two_factor_authentication
pub async fn show(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Status>> {
    let (_, user_id) = signed_in(auth, "read:accounts")?;
    Ok(Json(status(&state, user_id).await?))
}

#[derive(Debug, Default, Deserialize)]
pub struct Challenge {
    pub password: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Setup {
    /// The plain-text secret, for typing in by hand.
    pub secret: String,
    /// `otpauth://` URI, what the QR code carries.
    pub provisioning_uri: String,
    /// The QR code, as SVG.
    pub qr_code: String,
}

fn setup_key(state: &AppState, user_id: i64) -> String {
    state.redis_keys.key(format!("otp_setup:{user_id}"))
}

/// POST /api/eunha/v1/two_factor_authentication/otp
///
/// `OtpAuthenticationController#create`: a new secret, held until its first
/// code confirms it (`session[:new_otp_secret]`).
pub async fn begin_otp(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Challenge>,
) -> AppResult<Json<Setup>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    require_challenge(&state, user_id, form.password.as_deref()).await?;
    let user = user(&state, user_id).await?;
    if user.otp_enabled() {
        return Err(AppError::Unprocessable(
            "Two-factor authentication is enabled".into(),
        ));
    }
    if state.encryptor.is_none() {
        return Err(AppError::ServiceUnavailable(
            "Two-factor authentication needs the instance's ActiveRecord encryption keys".into(),
        ));
    }
    let secret = two_factor::generate_otp_secret();
    let mut redis = state.redis_coordination.clone();
    redis::cmd("SET")
        .arg(setup_key(&state, user_id))
        .arg(&secret)
        .arg("EX")
        .arg(SETUP_TTL_SECONDS)
        .query_async::<()>(&mut redis)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let provisioning_uri =
        two_factor::provisioning_uri(&secret, &user.email, &state.instance.domain);
    Ok(Json(Setup {
        qr_code: two_factor::qr_code_svg(&provisioning_uri),
        secret,
        provisioning_uri,
    }))
}

#[derive(Debug, Deserialize)]
pub struct Confirmation {
    pub otp_attempt: String,
}

#[derive(Debug, Serialize)]
pub struct RecoveryCodes {
    pub recovery_codes: Vec<String>,
}

/// POST /api/eunha/v1/two_factor_authentication/otp/confirm
///
/// `ConfirmationsController#create`: the first code from the authenticator,
/// then TOTP on and the recovery codes, shown this once.
pub async fn confirm_otp(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Confirmation>,
) -> AppResult<Json<RecoveryCodes>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    let key = setup_key(&state, user_id);
    let mut redis = state.redis_coordination.clone();
    let secret: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    // `ensure_otp_secret`.
    let secret = secret.ok_or_else(|| {
        AppError::Unprocessable("Begin two-factor authentication setup first".into())
    })?;
    let user = user(&state, user_id).await?;
    if !two_factor::validate_and_consume_otp(&state, &user, &form.otp_attempt, Some(&secret)).await
    {
        return Err(AppError::Unprocessable(
            "The entered code was invalid! Are server time and device time correct?".into(),
        ));
    }
    let codes = two_factor::enable(&state, user_id, &secret).await?;
    let _: redis::RedisResult<()> = redis::cmd("DEL").arg(&key).query_async(&mut redis).await;
    two_factor::notify(
        &state,
        &user,
        NoticeKind::Security(OwnedNotice::TwoFactorEnabled),
    );
    Ok(Json(RecoveryCodes {
        recovery_codes: codes,
    }))
}

/// POST /api/eunha/v1/two_factor_authentication/recovery_codes
///
/// `RecoveryCodesController#create`.
pub async fn regenerate_recovery_codes(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Challenge>,
) -> AppResult<Json<RecoveryCodes>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    require_challenge(&state, user_id, form.password.as_deref()).await?;
    let user = user(&state, user_id).await?;
    if !user.otp_enabled() {
        return Err(AppError::Unprocessable(
            "Two-factor authentication is not enabled".into(),
        ));
    }
    let codes = two_factor::regenerate_backup_codes(&state, user_id).await?;
    two_factor::notify(
        &state,
        &user,
        NoticeKind::Security(OwnedNotice::TwoFactorRecoveryCodesChanged),
    );
    Ok(Json(RecoveryCodes {
        recovery_codes: codes,
    }))
}

/// DELETE /api/eunha/v1/two_factor_authentication
///
/// `TwoFactorAuthenticationMethodsController#disable`: `disable_two_factor!`,
/// which removes the security keys too, and the mail.
pub async fn disable(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Challenge>,
) -> AppResult<Json<Status>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    require_challenge(&state, user_id, form.password.as_deref()).await?;
    let user = user(&state, user_id).await?;
    // `require_otp_enabled`.
    if !user.otp_enabled() {
        return Err(AppError::Unprocessable(
            "Two-factor authentication is not enabled".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    two_factor::disable(&mut tx, user_id).await?;
    tx.commit().await?;
    two_factor::notify(&state, &user, NoticeKind::TwoFactorDisabled);
    Ok(Json(status(&state, user_id).await?))
}

fn webauthn_challenge_key(state: &AppState, user_id: i64) -> String {
    state
        .redis_keys
        .key(format!("webauthn_registration:{user_id}"))
}

/// `redirect_invalid_otp`: security keys come after an authenticator app.
fn require_otp(user: &UserTwoFactor) -> AppResult<()> {
    if user.otp_enabled() {
        Ok(())
    } else {
        Err(AppError::Unprocessable(
            "To use security keys please enable two-factor authentication first.".into(),
        ))
    }
}

/// POST /api/eunha/v1/two_factor_authentication/webauthn_credentials/options
///
/// `WebauthnCredentialsController#options`: what the browser needs to make a
/// key, with `users.webauthn_id` assigned on first use.
pub async fn webauthn_options(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Challenge>,
) -> AppResult<Json<serde_json::Value>> {
    let (account_id, user_id) = signed_in(auth, "write:accounts")?;
    require_challenge(&state, user_id, form.password.as_deref()).await?;
    require_otp(&user(&state, user_id).await?)?;
    let handle = sqlx::query_scalar!(
        r#"UPDATE users SET webauthn_id = COALESCE(webauthn_id, $1)
           WHERE id = $2 RETURNING webauthn_id AS "webauthn_id!""#,
        crate::webauthn::generate_user_id(),
        user_id,
    )
    .fetch_one(&state.db)
    .await?;
    let username = sqlx::query_scalar!("SELECT username FROM accounts WHERE id = $1", account_id)
        .fetch_one(&state.db)
        .await?;
    let exclude: Vec<String> = sqlx::query_scalar!(
        "SELECT external_id FROM webauthn_credentials WHERE user_id = $1 ORDER BY id",
        user_id,
    )
    .fetch_all(&state.db)
    .await?;
    let challenge = crate::webauthn::generate_challenge();
    let mut redis = state.redis_coordination.clone();
    redis::cmd("SET")
        .arg(webauthn_challenge_key(&state, user_id))
        .arg(&challenge)
        .arg("EX")
        .arg(SETUP_TTL_SECONDS)
        .query_async::<()>(&mut redis)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(Json(crate::webauthn::creation_options(
        &challenge, &username, &handle, &exclude,
    )))
}

#[derive(Debug, Deserialize)]
pub struct NewKey {
    pub credential: serde_json::Value,
    pub nickname: Option<String>,
}

/// POST /api/eunha/v1/two_factor_authentication/webauthn_credentials
///
/// `WebauthnCredentialsController#create`.
pub async fn add_webauthn_credential(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<NewKey>,
) -> AppResult<Json<WebauthnCredential>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    let user = user(&state, user_id).await?;
    require_otp(&user)?;
    let key = webauthn_challenge_key(&state, user_id);
    let mut redis = state.redis_coordination.clone();
    let challenge: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let error = || {
        AppError::UnauthorizedMsg(
            "There was a problem adding your security key. Please try again.".into(),
        )
    };
    let challenge = challenge.ok_or_else(error)?;
    let rp = crate::webauthn::RelyingParty::for_domain(&state.instance.domain);
    let new = crate::webauthn::verify_registration(&form.credential, &challenge, &rp)
        .map_err(|_| error())?;
    let _: redis::RedisResult<()> = redis::cmd("DEL").arg(&key).query_async(&mut redis).await;

    // `validates :nickname, presence: true, uniqueness: { scope: :user_id }`
    // and `validates :external_id, uniqueness: true`.
    let nickname = form.nickname.unwrap_or_default().trim().to_string();
    let refused = || {
        AppError::Unprocessable(
            "There was a problem adding your security key. Please try again.".into(),
        )
    };
    if nickname.is_empty() {
        return Err(refused());
    }
    let row = sqlx::query!(
        r#"INSERT INTO webauthn_credentials
             (external_id, public_key, nickname, sign_count, user_id, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), now())
           ON CONFLICT DO NOTHING
           RETURNING id, nickname, created_at"#,
        new.external_id,
        new.public_key,
        nickname,
        new.sign_count,
        user_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(refused)?;

    // The first key is `webauthn_enabled`; the rest are each announced.
    let notice = if user.webauthn_credentials == 0 {
        OwnedNotice::WebauthnEnabled
    } else {
        OwnedNotice::WebauthnCredentialAdded(row.nickname.clone())
    };
    two_factor::notify(&state, &user, NoticeKind::Security(notice));
    Ok(Json(WebauthnCredential {
        id: row.id.to_string(),
        nickname: row.nickname,
        created_at: timestamp(row.created_at),
    }))
}

/// DELETE /api/eunha/v1/two_factor_authentication/webauthn_credentials/:id
///
/// `WebauthnCredentialsController#destroy`.
pub async fn remove_webauthn_credential(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
    Params(form): Params<Challenge>,
) -> AppResult<Json<Status>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    require_challenge(&state, user_id, form.password.as_deref()).await?;
    let user = user(&state, user_id).await?;
    require_otp(&user)?;
    let nickname = sqlx::query_scalar!(
        "DELETE FROM webauthn_credentials WHERE id = $1 AND user_id = $2 RETURNING nickname",
        id,
        user_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let notice = if user.webauthn_credentials <= 1 {
        OwnedNotice::WebauthnDisabled
    } else {
        OwnedNotice::WebauthnCredentialDeleted(nickname)
    };
    two_factor::notify(&state, &user, NoticeKind::Security(notice));
    Ok(Json(status(&state, user_id).await?))
}
