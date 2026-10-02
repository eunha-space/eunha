//! Two-factor authentication as Mastodon has it through devise-two-factor:
//! TOTP with `users.otp_secret`, single-use recovery codes in
//! `users.otp_backup_codes`, and `users.consumed_timestep` to refuse a code
//! that was already used. Security keys are in [`crate::webauthn`].
//!
//! Stored the way Mastodon 4.7 stores them, so either can read what the other
//! wrote:
//!
//!  -  `otp_secret` is a base32 secret behind `encrypts :otp_secret`, which is
//!     Rails' `ActiveRecord::Encryption` envelope ([`crate::rails_encryption`]).
//!     Without the encryption keys eunha can neither enable TOTP nor check a
//!     code, as Mastodon cannot.
//!  -  `otp_backup_codes` holds bcrypt digests (`Devise::Encryptor`, ten
//!     stretches) of ten 16-character hex codes.

use hmac::{Hmac, Mac};
use rand::RngCore as _;
use sqlx::PgPool;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// ROTP's defaults, which devise-two-factor does not change.
const OTP_INTERVAL: i64 = 30;
const OTP_DIGITS: u32 = 6;
/// devise-two-factor's `otp_allowed_drift`, in seconds either way.
const OTP_ALLOWED_DRIFT: i64 = 30;
/// `devise :two_factor_authenticatable, otp_secret_length: 32`, in bytes.
const OTP_SECRET_BYTES: usize = 32;
/// `devise :two_factor_backupable, otp_number_of_backup_codes: 10`.
const BACKUP_CODE_COUNT: usize = 10;
/// devise-two-factor's `otp_backup_code_length`, in hex characters.
const BACKUP_CODE_LENGTH: usize = 16;
/// Devise's `stretches` outside tests.
const BCRYPT_COST: u32 = 10;
/// `Auth::SessionsController::MAX_2FA_ATTEMPTS_PER_HOUR`.
pub const MAX_2FA_ATTEMPTS_PER_HOUR: i64 = 10;

// ── TOTP ────────────────────────────────────────────────────────────────────

/// `User.generate_otp_secret`: `ROTP::Base32.random(32)`.
pub fn generate_otp_secret() -> String {
    let mut bytes = [0u8; OTP_SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    data_encoding::BASE32_NOPAD.encode(&bytes)
}

/// The bytes of a base32 secret, ignoring case, spaces and padding as ROTP does.
fn decode_secret(secret: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bytes = Vec::with_capacity(secret.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in secret.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let value = ALPHABET
            .iter()
            .position(|a| *a == c.to_ascii_uppercase() as u8)?;
        buffer = (buffer << 5) | value as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // Leftover bits short of a byte are dropped, as ROTP drops them.
    Some(bytes)
}

/// RFC 4226 HOTP: the code for `counter`.
fn hotp(key: &[u8], counter: u64) -> String {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    format!(
        "{:0width$}",
        binary % 10u32.pow(OTP_DIGITS),
        width = OTP_DIGITS as usize
    )
}

/// The time step a moment falls in.
pub fn timestep(unix_seconds: i64) -> i64 {
    unix_seconds.div_euclid(OTP_INTERVAL)
}

/// The TOTP code for `secret` at `unix_seconds`.
pub fn totp_at(secret: &str, unix_seconds: i64) -> Option<String> {
    let key = decode_secret(secret)?;
    Some(hotp(&key, timestep(unix_seconds) as u64))
}

/// `ROTP::TOTP#verify(code, drift_behind:, drift_ahead:, after:)`: the time
/// step `code` belongs to, within the allowed drift and strictly after the
/// step last consumed.
fn verify_totp(secret: &str, code: &str, now: i64, after_step: Option<i64>) -> Option<i64> {
    let key = decode_secret(secret)?;
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.is_empty() {
        return None;
    }
    let first = timestep(now - OTP_ALLOWED_DRIFT);
    let last = timestep(now + OTP_ALLOWED_DRIFT);
    (first..=last)
        .filter(|step| after_step.is_none_or(|after| *step > after))
        .find(|step| constant_time_eq(hotp(&key, *step as u64).as_bytes(), code.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `User#otp_provisioning_uri(email, issuer: local_domain)`, as ROTP builds it.
pub fn provisioning_uri(secret: &str, account: &str, issuer: &str) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}",
        urlencoding::encode(issuer),
        urlencoding::encode(account),
        secret,
        urlencoding::encode(issuer),
    )
}

/// The provisioning URI as an SVG QR code, as `RQRCode::QRCode#as_svg` gives
/// Mastodon's setup page.
pub fn qr_code_svg(data: &str) -> String {
    match qrcode::QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color<'_>>()
            .min_dimensions(200, 200)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

// ── The user's state ────────────────────────────────────────────────────────

/// What a user has of two-factor authentication.
#[derive(Debug, Clone)]
pub struct UserTwoFactor {
    pub user_id: i64,
    pub email: String,
    pub otp_required_for_login: bool,
    otp_secret: Option<String>,
    pub otp_backup_codes: Vec<String>,
    consumed_timestep: Option<i32>,
    pub webauthn_credentials: i64,
    /// `users.updated_at`, which a pending sign-in compares against, as
    /// Mastodon's `session[:attempt_user_updated_at]` does.
    pub updated_at: chrono::NaiveDateTime,
    /// The role asks for it (`UserRole#require_2fa`).
    pub role_requires: bool,
    /// `Account#memorial?`, which keeps `UserMailer` from writing.
    pub memorial: bool,
    /// `users.time_zone`, which the times in its mail are written in.
    pub time_zone: Option<String>,
}

impl UserTwoFactor {
    /// `User#otp_enabled?`.
    pub fn otp_enabled(&self) -> bool {
        self.otp_required_for_login
    }

    /// `User#webauthn_enabled?`.
    pub fn webauthn_enabled(&self) -> bool {
        self.webauthn_credentials > 0
    }

    /// `User#two_factor_enabled?`.
    pub fn enabled(&self) -> bool {
        self.otp_enabled() || self.webauthn_enabled()
    }

    /// `User#missing_2fa?`.
    pub fn missing(&self) -> bool {
        !self.enabled() && self.role_requires
    }
}

/// Load a user's two-factor state.
pub async fn load(db: &PgPool, user_id: i64) -> sqlx::Result<Option<UserTwoFactor>> {
    let row = sqlx::query!(
        r#"SELECT u.id, u.email, u.otp_required_for_login, u.otp_secret, u.otp_backup_codes,
                  u.consumed_timestep, u.updated_at, a.memorial, u.time_zone,
                  (SELECT count(*) FROM webauthn_credentials w WHERE w.user_id = u.id) AS "webauthn!",
                  COALESCE((SELECT r.require_2fa FROM user_roles r WHERE r.id = COALESCE(u.role_id, -99)),
                           false) AS "role_requires!"
           FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.id = $1"#,
        user_id,
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| UserTwoFactor {
        user_id: r.id,
        email: r.email,
        otp_required_for_login: r.otp_required_for_login,
        otp_secret: r.otp_secret,
        otp_backup_codes: r.otp_backup_codes.unwrap_or_default(),
        consumed_timestep: r.consumed_timestep,
        webauthn_credentials: r.webauthn,
        updated_at: r.updated_at,
        role_requires: r.role_requires,
        memorial: r.memorial,
        time_zone: r.time_zone,
    }))
}

/// Seal a secret for `users.otp_secret`.
fn seal_secret(state: &AppState, secret: &str) -> AppResult<String> {
    let encryptor = state.encryptor.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailable(
            "Two-factor authentication needs the instance's ActiveRecord encryption keys".into(),
        )
    })?;
    encryptor
        .encrypt(secret)
        .map_err(|e| AppError::Internal(e.context("sealing an OTP secret")))
}

/// Open `users.otp_secret`. `None` when it cannot be read, which Mastodon
/// treats as a wrong code (`rescue OpenSSL::Cipher::CipherError`).
fn open_secret(state: &AppState, sealed: &str) -> Option<String> {
    state.encryptor.as_ref()?.decrypt(sealed).ok()
}

/// `User#validate_and_consume_otp!(code, otp_secret:)`: whether `code` is the
/// current code for the user's secret, or for `secret` when given (a secret
/// being confirmed). A good code is consumed, so it cannot be used twice.
pub async fn validate_and_consume_otp(
    state: &AppState,
    user: &UserTwoFactor,
    code: &str,
    secret: Option<&str>,
) -> bool {
    let secret = match secret {
        Some(secret) => Some(secret.to_owned()),
        None => user
            .otp_secret
            .as_deref()
            .and_then(|sealed| open_secret(state, sealed)),
    };
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    let after = user.consumed_timestep.map(i64::from);
    if verify_totp(&secret, code, now, after).is_none() {
        return false;
    }
    // `consume_otp!`: the current step is recorded, and a code is refused if
    // the current step was already recorded.
    let current = timestep(now) as i32;
    if user.consumed_timestep == Some(current) {
        return false;
    }
    sqlx::query!(
        "UPDATE users SET consumed_timestep = $1, updated_at = now() WHERE id = $2",
        current,
        user.user_id,
    )
    .execute(&state.db)
    .await
    .is_ok()
}

// ── Recovery codes ──────────────────────────────────────────────────────────

/// `User#generate_otp_backup_codes!`: ten fresh codes, and their digests.
async fn generate_backup_codes() -> AppResult<(Vec<String>, Vec<String>)> {
    crate::tenants::spawn_blocking(|| {
        let mut rng = rand::rng();
        let codes: Vec<String> = (0..BACKUP_CODE_COUNT)
            .map(|_| {
                let mut bytes = [0u8; BACKUP_CODE_LENGTH / 2];
                rng.fill_bytes(&mut bytes);
                hex::encode(bytes)
            })
            .collect();
        let digests = codes
            .iter()
            .map(|code| bcrypt::hash(code, BCRYPT_COST))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("hashing recovery codes: {e}")))?;
        Ok((codes, digests))
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("recovery codes did not finish: {e}")))?
}

/// `User#invalidate_otp_backup_code!(code)`: whether `code` is one of the
/// user's recovery codes, which it then stops being.
pub async fn invalidate_backup_code(state: &AppState, user: &UserTwoFactor, code: &str) -> bool {
    if code.is_empty() || user.otp_backup_codes.is_empty() {
        return false;
    }
    let digests = user.otp_backup_codes.clone();
    let attempt = code.to_owned();
    let matched = crate::tenants::spawn_blocking(move || {
        digests
            .iter()
            .position(|digest| bcrypt::verify(&attempt, digest).unwrap_or(false))
    })
    .await
    .ok()
    .flatten();
    let Some(index) = matched else {
        return false;
    };
    let mut remaining = user.otp_backup_codes.clone();
    remaining.remove(index);
    sqlx::query!(
        "UPDATE users SET otp_backup_codes = $1, updated_at = now() WHERE id = $2",
        &remaining,
        user.user_id,
    )
    .execute(&state.db)
    .await
    .is_ok()
}

// ── Changing it ─────────────────────────────────────────────────────────────

/// `Settings::TwoFactorAuthentication::ConfirmationsController#create`, past
/// the code check: TOTP on with `secret`, and fresh recovery codes returned.
pub async fn enable(state: &AppState, user_id: i64, secret: &str) -> AppResult<Vec<String>> {
    let sealed = seal_secret(state, secret)?;
    let (codes, digests) = generate_backup_codes().await?;
    sqlx::query!(
        r#"UPDATE users SET otp_required_for_login = true, otp_secret = $1,
                  otp_backup_codes = $2, updated_at = now()
           WHERE id = $3"#,
        sealed,
        &digests,
        user_id,
    )
    .execute(&state.db)
    .await?;
    Ok(codes)
}

/// `Settings::TwoFactorAuthentication::RecoveryCodesController#create`.
pub async fn regenerate_backup_codes(state: &AppState, user_id: i64) -> AppResult<Vec<String>> {
    let (codes, digests) = generate_backup_codes().await?;
    sqlx::query!(
        "UPDATE users SET otp_backup_codes = $1, updated_at = now() WHERE id = $2",
        &digests,
        user_id,
    )
    .execute(&state.db)
    .await?;
    Ok(codes)
}

/// `User#disable_two_factor!`: TOTP off, the secret and recovery codes gone,
/// and every security key removed.
pub async fn disable(conn: &mut sqlx::PgConnection, user_id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE users SET otp_required_for_login = false, otp_secret = NULL,
                  otp_backup_codes = CASE WHEN otp_backup_codes IS NULL THEN NULL ELSE '{}'::varchar[] END,
                  updated_at = now()
           WHERE id = $1"#,
        user_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM webauthn_credentials WHERE user_id = $1",
        user_id
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Mail one of the security notices, unless `UserMailer`'s
/// `active_for_authentication?` guard would keep it back.
pub fn notify(state: &AppState, user: &UserTwoFactor, notice: NoticeKind) {
    if user.memorial {
        return;
    }
    let email = state.email.clone();
    let to = user.email.clone();
    let domain = state.instance.domain.clone();
    crate::tenants::spawn(async move {
        let result = match &notice {
            NoticeKind::TwoFactorDisabled => email.send_two_factor_disabled(&to, &domain).await,
            NoticeKind::Security(kind) => {
                email
                    .send_security_notice(&to, &domain, kind.as_notice())
                    .await
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "could not send an account security email");
        }
    });
}

/// What [`notify`] sends.
#[derive(Debug, Clone)]
pub enum NoticeKind {
    TwoFactorDisabled,
    Security(OwnedNotice),
}

/// [`crate::email::SecurityNotice`] with its nickname owned, to cross a task.
#[derive(Debug, Clone)]
pub enum OwnedNotice {
    TwoFactorEnabled,
    TwoFactorRecoveryCodesChanged,
    WebauthnEnabled,
    WebauthnDisabled,
    WebauthnCredentialAdded(String),
    WebauthnCredentialDeleted(String),
    PasswordChange,
    /// The address being changed to.
    EmailChanged(String),
}

impl OwnedNotice {
    fn as_notice(&self) -> crate::email::SecurityNotice<'_> {
        use crate::email::SecurityNotice as N;
        match self {
            Self::TwoFactorEnabled => N::TwoFactorEnabled,
            Self::TwoFactorRecoveryCodesChanged => N::TwoFactorRecoveryCodesChanged,
            Self::WebauthnEnabled => N::WebauthnEnabled,
            Self::WebauthnDisabled => N::WebauthnDisabled,
            Self::WebauthnCredentialAdded(n) => N::WebauthnCredentialAdded(n),
            Self::WebauthnCredentialDeleted(n) => N::WebauthnCredentialDeleted(n),
            Self::PasswordChange => N::PasswordChange,
            Self::EmailChanged(e) => N::EmailChanged(e),
        }
    }
}

// ── Sign-in bookkeeping ─────────────────────────────────────────────────────

/// `check_second_factor_rate_limits`: count this attempt, and say whether the
/// user has now made too many this hour.
pub async fn second_factor_rate_limited(state: &AppState, user_id: i64) -> bool {
    let key = attempts_key(state, user_id);
    let mut redis = state.redis_coordination.clone();
    let attempts: i64 = match redis::cmd("INCRBY")
        .arg(&key)
        .arg(1)
        .query_async(&mut redis)
        .await
    {
        Ok(n) => n,
        Err(error) => {
            tracing::warn!(%error, "could not count second-factor attempts");
            return false;
        }
    };
    let _: redis::RedisResult<()> = redis::cmd("EXPIRE")
        .arg(&key)
        .arg(3600)
        .query_async(&mut redis)
        .await;
    attempts >= MAX_2FA_ATTEMPTS_PER_HOUR
}

/// `clear_2fa_attempt_from_user`.
pub async fn clear_second_factor_attempts(state: &AppState, user_id: i64) {
    let mut redis = state.redis_coordination.clone();
    let _: redis::RedisResult<()> = redis::cmd("DEL")
        .arg(attempts_key(state, user_id))
        .query_async(&mut redis)
        .await;
}

fn attempts_key(state: &AppState, user_id: i64) -> String {
    let hour = chrono::Timelike::hour(&chrono::Utc::now());
    state
        .redis_keys
        .key(format!("2fa_auth_attempts:{user_id}:{hour}"))
}

/// The tail of `on_authentication_failure`: `UserMailer.failed_2fa`, at most
/// once an hour.
pub async fn notify_failed_second_factor(
    state: &AppState,
    user: &UserTwoFactor,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) {
    let mut redis = state.redis_coordination.clone();
    let fresh: redis::RedisResult<Option<String>> = redis::cmd("SET")
        .arg(
            state
                .redis_keys
                .key(format!("2fa_failure_notification:{}", user.user_id)),
        )
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(3600)
        .query_async(&mut redis)
        .await;
    if !matches!(fresh, Ok(Some(_))) {
        return;
    }
    let email = state.email.clone();
    let to = user.email.clone();
    let domain = state.instance.domain.clone();
    let ip = ip.map(|ip| ip.to_string()).unwrap_or_default();
    let browser = crate::browser_detection::describe(user_agent.unwrap_or(""));
    let time_zone = user.time_zone.clone();
    crate::tenants::spawn(async move {
        if let Err(error) = email
            .send_sign_in_alert(
                &to,
                &domain,
                false,
                &ip,
                &browser,
                chrono::Utc::now(),
                time_zone.as_deref(),
            )
            .await
        {
            tracing::warn!(%error, "could not send a failed second factor email");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238's SHA-1 test vectors, truncated to six digits.
    #[test]
    fn matches_the_rfc_vectors() {
        let secret = data_encoding::BASE32_NOPAD.encode(b"12345678901234567890");
        assert_eq!(totp_at(&secret, 59).as_deref(), Some("287082"));
        assert_eq!(totp_at(&secret, 1_111_111_109).as_deref(), Some("081804"));
        assert_eq!(totp_at(&secret, 1_234_567_890).as_deref(), Some("005924"));
        assert_eq!(totp_at(&secret, 2_000_000_000).as_deref(), Some("279037"));
    }

    #[test]
    fn verifies_within_the_drift_and_after_the_consumed_step() {
        let secret = generate_otp_secret();
        assert_eq!(secret.len(), 52);
        let now = 1_700_000_015;
        let code = totp_at(&secret, now).unwrap();
        assert_eq!(verify_totp(&secret, &code, now, None), Some(timestep(now)));
        // ROTP strips whitespace from what was typed.
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert!(verify_totp(&secret, &spaced, now, None).is_some());
        // A step either side is accepted, two are not.
        let previous = totp_at(&secret, now - 30).unwrap();
        assert!(verify_totp(&secret, &previous, now, None).is_some());
        let stale = totp_at(&secret, now - 90).unwrap();
        assert!(verify_totp(&secret, &stale, now, None).is_none() || stale == code);
        // A consumed step is not accepted again.
        assert!(verify_totp(&secret, &code, now, Some(timestep(now))).is_none());
        assert!(verify_totp(&secret, "", now, None).is_none());
    }

    #[test]
    fn decodes_padded_and_lowercase_secrets() {
        let secret = data_encoding::BASE32.encode(b"0123456789abcdefghijklmnopqrstuv");
        assert!(secret.ends_with('='));
        assert_eq!(
            decode_secret(&secret.to_lowercase()).unwrap(),
            b"0123456789abcdefghijklmnopqrstuv"
        );
    }

    #[test]
    fn builds_the_provisioning_uri() {
        assert_eq!(
            provisioning_uri("ABC", "a@b.example", "b.example"),
            "otpauth://totp/b.example:a%40b.example?secret=ABC&issuer=b.example"
        );
        assert!(qr_code_svg("otpauth://totp/x").starts_with("<?xml"));
    }
}
