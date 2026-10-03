use crate::error::{AppError, AppResult};

/// Run deliberately expensive work — a password hash takes tens of
/// milliseconds of CPU — on Tokio's blocking pool. On a worker it would stall
/// every request scheduled there for as long, and in a process serving several
/// instances those are other tenants' requests too.
async fn off_the_runtime<T: Send + 'static>(
    what: &'static str,
    work: impl FnOnce() -> AppResult<T> + Send + 'static,
) -> AppResult<T> {
    crate::tenants::spawn_blocking(work)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{what} did not finish: {e}")))?
}

/// Check `password` against a bcrypt or argon2 `hash`, off the async runtime.
pub async fn verify_password(password: &str, hash: &str) -> AppResult<()> {
    let (password, hash) = (password.to_owned(), hash.to_owned());
    off_the_runtime("password check", move || {
        verify_password_blocking(&password, &hash)
    })
    .await
}

/// Hash a new password with argon2, off the async runtime.
pub async fn hash_password(password: &str) -> AppResult<String> {
    let password = password.to_owned();
    off_the_runtime("password hashing", move || {
        hash_password_blocking(&password)
    })
    .await
}

fn verify_password_blocking(password: &str, hash: &str) -> AppResult<()> {
    if hash.starts_with("$2a$") || hash.starts_with("$2b$") || hash.starts_with("$2y$") {
        let ok = bcrypt::verify(password, hash)
            .map_err(|_| AppError::Internal(anyhow::anyhow!("bcrypt error")))?;
        if ok {
            Ok(())
        } else {
            Err(AppError::Unauthorized)
        }
    } else {
        use argon2::PasswordVerifier;
        let parsed = argon2::PasswordHash::new(hash)
            .map_err(|_| AppError::Internal(anyhow::anyhow!("invalid password hash")))?;
        argon2::Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| AppError::Unauthorized)
    }
}

pub fn generate_token(len: usize) -> String {
    use rand::RngCore;
    let mut rng = rand::rng();
    (0..len)
        .map(|_| format!("{:02x}", rng.next_u32() as u8))
        .collect()
}

fn hash_password_blocking(password: &str) -> AppResult<String> {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::{Argon2, PasswordHasher};

    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("password hashing failed: {e}")))
}

/// Rails' `message_verifier(purpose)` without `SECRET_KEY_BASE`, which eunha
/// does not have: HMAC-SHA256 under a key derived for `purpose` from the
/// instance's VAPID private key, as `base64url(message)--hexdigest` so it can
/// sit in a path or a query string. `MessageVerifier#generate`.
pub fn sign_message(secret: &str, purpose: &[u8], message: &str) -> String {
    use base64::Engine;
    use hmac::Mac;
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(message);
    let mut mac = message_signer(secret, purpose);
    mac.update(data.as_bytes());
    format!("{data}--{}", hex::encode(mac.finalize().into_bytes()))
}

/// `MessageVerifier#verify`: the message [`sign_message`] signed for
/// `purpose`, or `None` for anything else.
pub fn verify_message(secret: &str, purpose: &[u8], signed: &str) -> Option<String> {
    use base64::Engine;
    use hmac::Mac;
    let (data, digest) = signed.rsplit_once("--")?;
    let digest = hex::decode(digest).ok()?;
    let mut mac = message_signer(secret, purpose);
    mac.update(data.as_bytes());
    mac.verify_slice(&digest).ok()?;
    let message = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .ok()?;
    String::from_utf8(message).ok()
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// A key for this purpose rather than the secret itself, as Rails derives one
/// per verifier name from its secret.
fn message_signer(secret: &str, purpose: &[u8]) -> HmacSha256 {
    use hmac::Mac;
    let mut derive =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any length");
    derive.update(purpose);
    let key = derive.finalize().into_bytes();
    HmacSha256::new_from_slice(&key).expect("HMAC takes a key of any length")
}
