//! What Mastodon signs and digests with Rails' `secret_key_base`.
//!
//! An instance configured with its Mastodon's `SECRET_KEY_BASE`
//! (`instance.secret_key_base`) mints and accepts exactly what that Mastodon
//! does: async refresh ids, the signed GlobalIDs in unsubscribe links,
//! Devise's digest of a password reset token, and the `SELF_DESTRUCT` value. Without it, eunha keeps its own
//! schemes, which no Mastodon reads (see the divergences that name
//! `secret_key_base`).
//!
//! Under Mastodon 4.7.1's `config.load_defaults 8.1`:
//!
//!  -  `Rails.application.key_generator` is PBKDF2-HMAC-SHA256 over the secret,
//!     1000 iterations, 64 bytes, salted with what the key is for.
//!  -  `Rails.application.message_verifier(name)` is an
//!     `ActiveSupport::MessageVerifier` under the key for `name`: the value
//!     serialized as JSON (`:json_allow_marshal`), with any purpose and expiry
//!     inside it as `{"_rails":{"data":…,"exp":…,"pur":…}}`
//!     (`use_message_serializer_for_metadata`), in strict base64, then `--`
//!     and the hex HMAC-SHA1 of that base64.
//!  -  A `SignedGlobalID` is the same under the key for `signed_global_ids`,
//!     except that `GlobalID::Verifier` writes URL-safe base64 with padding.
//!     The value is `gid://mastodon/<Model>/<id>`, and it expires a month
//!     after it was made.
//!  -  `Devise.token_generator` keeps HMAC-SHA256 hex digests under a key per
//!     column. Devise builds its `ActiveSupport::KeyGenerator` in an
//!     initializer, before the `after_initialize` hook that makes SHA-256 the
//!     class default, so that key is PBKDF2-HMAC-SHA1 at KeyGenerator's own
//!     2^16 iterations.
//!
//! *scripts/rails_signing_vectors.rb* prints the vectors the tests below hold.

use std::sync::{Arc, OnceLock};

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD, URL_SAFE};
use base64::engine::DecodePaddingMode;
use base64::Engine;
use chrono::{DateTime, Months, Utc};
use hmac::Mac;
use serde::Deserialize;
use serde_json::Value;

type HmacSha1 = hmac::Hmac<sha1::Sha1>;
type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// `ActiveSupport::KeyGenerator#generate_key`'s default length.
const KEY_LEN: usize = 64;
/// The iterations `Rails.application.key_generator` asks for.
const APP_ITERATIONS: u32 = 1000;
/// `ActiveSupport::KeyGenerator`'s default iterations, which Devise gets.
const DEVISE_ITERATIONS: u32 = 1 << 16;
/// `GlobalID.app` for `Mastodon::Application`.
const GLOBAL_ID_APP: &str = "mastodon";
/// The hex length of an HMAC-SHA1, which is what `MessageVerifier` signs with.
const DIGEST_HEX_LEN: usize = 40;

/// A Mastodon's `SECRET_KEY_BASE`, and the keys derived from it as they are
/// first needed.
#[derive(Clone)]
pub struct SecretKeyBase(Arc<Inner>);

struct Inner {
    secret: String,
    async_refreshes: OnceLock<[u8; KEY_LEN]>,
    self_destruct: OnceLock<[u8; KEY_LEN]>,
    signed_global_ids: OnceLock<[u8; KEY_LEN]>,
    reset_password_token: OnceLock<[u8; KEY_LEN]>,
}

impl std::fmt::Debug for SecretKeyBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the secret.
        f.write_str("SecretKeyBase(..)")
    }
}

/// `instance.secret_key_base`, where a blank value — an empty environment
/// variable — is no secret at all.
pub fn deserialize_optional<'de, D>(deserializer: D) -> Result<Option<SecretKeyBase>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value
        .filter(|s| !s.trim().is_empty())
        .map(SecretKeyBase::new))
}

impl SecretKeyBase {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(Arc::new(Inner {
            secret: secret.into(),
            async_refreshes: OnceLock::new(),
            self_destruct: OnceLock::new(),
            signed_global_ids: OnceLock::new(),
            reset_password_token: OnceLock::new(),
        }))
    }

    /// `Rails.application.key_generator.generate_key(salt)`.
    fn app_key(&self, salt: &str) -> [u8; KEY_LEN] {
        let mut key = [0u8; KEY_LEN];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
            self.0.secret.as_bytes(),
            salt.as_bytes(),
            APP_ITERATIONS,
            &mut key,
        );
        key
    }

    fn async_refreshes(&self) -> Verifier<'_> {
        Verifier {
            key: self
                .0
                .async_refreshes
                .get_or_init(|| self.app_key("async_refreshes")),
            encoding: Encoding::Strict,
        }
    }

    fn self_destruct(&self) -> Verifier<'_> {
        Verifier {
            key: self
                .0
                .self_destruct
                .get_or_init(|| self.app_key(crate::self_destruct::VERIFY_PURPOSE)),
            encoding: Encoding::Strict,
        }
    }

    fn signed_global_ids(&self) -> Verifier<'_> {
        Verifier {
            key: self
                .0
                .signed_global_ids
                .get_or_init(|| self.app_key("signed_global_ids")),
            encoding: Encoding::UrlSafe,
        }
    }

    /// `AsyncRefresh#id`: `message_verifier('async_refreshes').generate(key)`.
    pub fn async_refresh_id(&self, redis_key: &str) -> String {
        self.async_refreshes()
            .generate(&Value::String(redis_key.to_owned()), None, None)
    }

    /// `AsyncRefresh.find`'s `message_verifier('async_refreshes').verify(id)`:
    /// the Redis key an id signs.
    pub fn verify_async_refresh_id(&self, id: &str) -> Option<String> {
        match self.async_refreshes().verify(id, None, Utc::now())? {
            Value::String(key) => Some(key),
            _ => None,
        }
    }

    /// `tootctl self-destruct`'s value:
    /// `message_verifier('self-destruct').generate(local_domain)`.
    pub fn self_destruct_value(&self, domain: &str) -> String {
        self.self_destruct()
            .generate(&Value::String(domain.to_owned()), None, None)
    }

    /// `SelfDestructHelper.self_destruct?`'s
    /// `message_verifier('self-destruct').verify(value)`: the domain a value
    /// signs.
    pub fn verify_self_destruct(&self, value: &str) -> Option<String> {
        match self.self_destruct().verify(value, None, Utc::now())? {
            Value::String(domain) => Some(domain),
            _ => None,
        }
    }

    /// `record.to_sgid(for: purpose)` for the record `model` `id`, made at
    /// `now` and good for `SignedGlobalID.expires_in`, a month.
    pub fn signed_global_id(
        &self,
        model: &str,
        id: i64,
        purpose: &str,
        now: DateTime<Utc>,
    ) -> String {
        let expires_at = now.checked_add_months(Months::new(1)).unwrap_or(now);
        self.signed_global_ids().generate(
            &Value::String(format!("gid://{GLOBAL_ID_APP}/{model}/{id}")),
            Some(purpose),
            Some(expires_at),
        )
    }

    /// `GlobalID::Locator.locate_signed(sgid, for: purpose)` up to the find:
    /// the model and id a genuine, unexpired signed GlobalID for `purpose`
    /// names. As `SignedGlobalID.verify` does, a GlobalID in the format older
    /// globalid releases wrote, with its purpose and expiry beside it, is read
    /// too.
    pub fn locate_signed(
        &self,
        sgid: &str,
        purpose: &str,
        now: DateTime<Utc>,
    ) -> Option<(String, i64)> {
        let verifier = self.signed_global_ids();
        let gid = match verifier.verify(sgid, Some(purpose), now) {
            Some(Value::String(gid)) => gid,
            Some(_) => return None,
            None => legacy_global_id(verifier.verify(sgid, None, now)?, purpose, now)?,
        };
        parse_global_id(&gid)
    }

    /// `Devise.token_generator.digest(User, :reset_password_token, token)`,
    /// what `users.reset_password_token` holds for a mailed token.
    ///
    /// The key takes 2^16 rounds of PBKDF2 to derive, so the first call runs
    /// that on the blocking pool and later ones reuse it.
    pub async fn reset_password_token_digest(&self, token: &str) -> String {
        let inner = Arc::clone(&self.0);
        let key = match inner.reset_password_token.get() {
            Some(key) => *key,
            None => {
                let derive = Arc::clone(&inner);
                let derived = crate::tenants::spawn_blocking(move || {
                    *derive
                        .reset_password_token
                        .get_or_init(|| devise_key(&derive.secret, "reset_password_token"))
                })
                .await;
                match derived {
                    Ok(key) => key,
                    Err(_) => *inner
                        .reset_password_token
                        .get_or_init(|| devise_key(&inner.secret, "reset_password_token")),
                }
            }
        };
        let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC takes a key of any length");
        mac.update(token.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The key `Devise::TokenGenerator#key_for(column)` asks its key generator
/// for: `"Devise #{column}"`.
fn devise_key(secret: &str, column: &str) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(
        secret.as_bytes(),
        format!("Devise {column}").as_bytes(),
        DEVISE_ITERATIONS,
        &mut key,
    );
    key
}

/// How a verifier writes its base64.
#[derive(Clone, Copy)]
enum Encoding {
    /// `MessageVerifier`'s default, `Base64.strict_encode64`.
    Strict,
    /// `GlobalID::Verifier`, `Base64.urlsafe_encode64` with its padding.
    UrlSafe,
}

/// An `ActiveSupport::MessageVerifier` as Mastodon configures one.
struct Verifier<'a> {
    key: &'a [u8; KEY_LEN],
    encoding: Encoding,
}

impl Verifier<'_> {
    /// `MessageVerifier#generate(value, purpose:, expires_at:)`.
    fn generate(
        &self,
        value: &Value,
        purpose: Option<&str>,
        expires_at: Option<DateTime<Utc>>,
    ) -> String {
        let serialized = if purpose.is_none() && expires_at.is_none() {
            active_support_json(value)
        } else {
            // `wrap_in_metadata_envelope`, which adds `exp` before `pur`.
            let mut hash = format!("{{\"data\":{}", active_support_json(value));
            if let Some(expires_at) = expires_at {
                hash.push_str(",\"exp\":");
                hash.push_str(&active_support_json(&Value::String(
                    expires_at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                )));
            }
            if let Some(purpose) = purpose {
                hash.push_str(",\"pur\":");
                hash.push_str(&active_support_json(&Value::String(purpose.to_owned())));
            }
            format!("{{\"_rails\":{hash}}}}}")
        };
        let encoded = match self.encoding {
            Encoding::Strict => STANDARD.encode(serialized),
            Encoding::UrlSafe => URL_SAFE.encode(serialized),
        };
        let digest = hex::encode(self.mac(&encoded).finalize().into_bytes());
        format!("{encoded}--{digest}")
    }

    /// `MessageVerifier#verified(message, purpose:)`: the value of a message
    /// this verifier signed, for `purpose` and not expired at `now`.
    fn verify(&self, signed: &str, purpose: Option<&str>, now: DateTime<Utc>) -> Option<Value> {
        // `extract_encoded`: the digest is the last 40 characters, after `--`.
        let split = signed.len().checked_sub(DIGEST_HEX_LEN + 2)?;
        let (encoded, rest) = (signed.get(..split)?, signed.get(split..)?);
        let digest = rest.strip_prefix("--")?;
        if encoded.is_empty()
            || !digest
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        self.mac(encoded)
            .verify_slice(&hex::decode(digest).ok()?)
            .ok()?;

        let serialized = decode_base64(encoded)?;
        // Marshal, which `:json_allow_marshal` would still read, and the
        // metadata envelope from before `use_message_serializer_for_metadata`,
        // are what a Mastodon older than 4.7.1's Rails wrote. Neither is minted
        // any more, and what they signed has expired since.
        if serialized.starts_with(b"\x04\x08")
            || serialized.starts_with(br#"{"_rails":{"message":"#)
        {
            return None;
        }
        let value: Value = serde_json::from_slice(&serialized).ok()?;
        match value.get("_rails") {
            Some(Value::Object(envelope)) if value.is_object() => {
                // `extract_from_metadata_envelope`.
                if let Some(exp) = envelope.get("exp").filter(|e| !e.is_null()) {
                    let exp = DateTime::parse_from_rfc3339(exp.as_str()?).ok()?;
                    if now >= exp {
                        return None;
                    }
                }
                let pur = match envelope.get("pur") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                };
                if pur != purpose.unwrap_or_default() {
                    return None;
                }
                Some(envelope.get("data").cloned().unwrap_or(Value::Null))
            }
            _ if purpose.is_none() => Some(value),
            _ => None,
        }
    }

    fn mac(&self, encoded: &str) -> HmacSha1 {
        let mut mac = HmacSha1::new_from_slice(self.key).expect("HMAC takes a key of any length");
        mac.update(encoded.as_bytes());
        mac
    }
}

/// Base64 in either alphabet, padded or not: `MessageVerifier#decode` tries
/// strict and then URL-safe, and `Base64.urlsafe_decode64` takes either.
fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let standard: String = encoded
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    LENIENT.decode(standard).ok()
}

/// `ActiveSupport::JSON.encode`: JSON, with `<`, `>`, `&` and the two Unicode
/// line separators escaped as `\uXXXX`.
fn active_support_json(value: &Value) -> String {
    let json = value.to_string();
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') {
            out.push('\\');
            out.push_str(&format!("u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `SignedGlobalID.verify_with_legacy_self_validated_metadata`: the GlobalID
/// of `{"gid":…,"purpose":…,"expires_at":…}` when it is for `purpose` and has
/// not expired.
fn legacy_global_id(metadata: Value, purpose: &str, now: DateTime<Utc>) -> Option<String> {
    if let Some(expires_at) = metadata.get("expires_at").and_then(Value::as_str) {
        if now > DateTime::parse_from_rfc3339(expires_at).ok()? {
            return None;
        }
    }
    if metadata.get("purpose").and_then(Value::as_str) != Some(purpose) {
        return None;
    }
    metadata.get("gid")?.as_str().map(str::to_owned)
}

/// `URI::GID`: the model name and id of `gid://<app>/<Model>/<id>`, whatever
/// the app, as `GlobalID::Locator` reads it.
fn parse_global_id(gid: &str) -> Option<(String, i64)> {
    let rest = gid.strip_prefix("gid://")?;
    let (app, path) = rest.split_once('/')?;
    if app.is_empty() {
        return None;
    }
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let (model, id) = path.split_once('/')?;
    if model.is_empty() {
        return None;
    }
    Some((model.to_owned(), id.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// What *scripts/rails_signing_vectors.rb* signs with when given nothing.
    fn secret() -> SecretKeyBase {
        SecretKeyBase::new("0123456789abcdef".repeat(8))
    }

    fn expires_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 2, 7, 25, 1).unwrap()
            + chrono::Duration::microseconds(123_456)
    }

    const ASYNC_REFRESH_ID: &str = "ImFzeW5jX3JlZnJlc2hlczp2MTphY2NvdW50czoxMjM6cmVmcmVzaF9mb2xsb3dlcnMi--509406b78242a360e365efdfc925d0e7c576618f";
    const USER_SGID: &str = "eyJfcmFpbHMiOnsiZGF0YSI6ImdpZDovL21hc3RvZG9uL1VzZXIvMSIsImV4cCI6IjIwMjYtMTEtMDJUMDc6MjU6MDEuMTIzWiIsInB1ciI6InVuc3Vic2NyaWJlIn19--daf96e133c227720ee735a3e422158b568435e39";
    const SUBSCRIPTION_SGID: &str = "eyJfcmFpbHMiOnsiZGF0YSI6ImdpZDovL21hc3RvZG9uL0VtYWlsU3Vic2NyaXB0aW9uLzQyIiwiZXhwIjoiMjAyNi0xMS0wMlQwNzoyNTowMS4xMjNaIiwicHVyIjoidW5zdWJzY3JpYmUifX0=--30f11c153b2ac1e70a2a9a0d7ab435ff60e4a55e";
    /// `message_verifier('self-destruct').generate('example.com')`.
    const SELF_DESTRUCT: &str = "ImV4YW1wbGUuY29tIg==--d98e70d05297ffc5945339fd0796bb8aafb0222f";
    const RESET_DIGEST: &str = "a5106b6b4dc0e29d5f7eef97ecf87a7734d60218ca010180d6cf0aea936391a7";

    #[test]
    fn async_refresh_ids_are_rails_message_verifier_output() {
        let skb = secret();
        let key = "async_refreshes:v1:accounts:123:refresh_followers";
        assert_eq!(skb.async_refresh_id(key), ASYNC_REFRESH_ID);
        assert_eq!(
            skb.verify_async_refresh_id(ASYNC_REFRESH_ID).as_deref(),
            Some(key)
        );
    }

    #[test]
    fn self_destruct_values_are_rails_message_verifier_output() {
        let skb = secret();
        assert_eq!(skb.self_destruct_value("example.com"), SELF_DESTRUCT);
        assert_eq!(
            skb.verify_self_destruct(SELF_DESTRUCT).as_deref(),
            Some("example.com")
        );
        assert_eq!(
            SecretKeyBase::new("another secret").verify_self_destruct(SELF_DESTRUCT),
            None
        );
    }

    #[test]
    fn async_refresh_ids_signed_with_another_secret_are_refused() {
        let other = SecretKeyBase::new("another secret");
        assert_eq!(other.verify_async_refresh_id(ASYNC_REFRESH_ID), None);
        let mut tampered = ASYNC_REFRESH_ID.to_owned();
        tampered.replace_range(0..1, "J");
        assert_eq!(secret().verify_async_refresh_id(&tampered), None);
    }

    #[test]
    fn signed_global_ids_are_what_globalid_writes() {
        let skb = secret();
        let made = expires_at() - Months::new(1);
        assert_eq!(
            skb.signed_global_id("User", 1, "unsubscribe", made),
            USER_SGID
        );
        assert_eq!(
            skb.signed_global_id("EmailSubscription", 42, "unsubscribe", made),
            SUBSCRIPTION_SGID
        );
    }

    #[test]
    fn signed_global_ids_are_located_until_they_expire() {
        let skb = secret();
        let before = expires_at() - chrono::Duration::days(1);
        assert_eq!(
            skb.locate_signed(USER_SGID, "unsubscribe", before),
            Some(("User".to_owned(), 1))
        );
        assert_eq!(
            skb.locate_signed(SUBSCRIPTION_SGID, "unsubscribe", before),
            Some(("EmailSubscription".to_owned(), 42))
        );
        // The digest covers the padding, so a link that lost it is refused.
        let unpadded = SUBSCRIPTION_SGID.replace("=--", "--");
        assert_eq!(skb.locate_signed(&unpadded, "unsubscribe", before), None);
        assert_eq!(
            skb.locate_signed(USER_SGID, "unsubscribe", expires_at()),
            None
        );
        assert_eq!(skb.locate_signed(USER_SGID, "default", before), None);
    }

    #[test]
    fn legacy_signed_global_ids_are_located() {
        let skb = secret();
        let now = expires_at() - chrono::Duration::days(1);
        let legacy = skb.signed_global_ids().generate(
            &serde_json::json!({
                "gid": "gid://mastodon/User/7",
                "purpose": "unsubscribe",
                "expires_at": "2026-11-02T07:25:01.123Z",
            }),
            None,
            None,
        );
        assert_eq!(
            skb.locate_signed(&legacy, "unsubscribe", now),
            Some(("User".to_owned(), 7))
        );
        assert_eq!(skb.locate_signed(&legacy, "other", now), None);
    }

    #[tokio::test]
    async fn reset_password_digests_are_devise_token_generator_output() {
        let skb = secret();
        assert_eq!(
            skb.reset_password_token_digest("sxyzAbCdEfGhIjKlMnOp")
                .await,
            RESET_DIGEST
        );
    }

    #[test]
    fn json_is_escaped_as_active_support_escapes_it() {
        assert_eq!(
            active_support_json(&Value::String("a<b>&\u{2028}/".into())),
            [r#""a"#, "\\u003cb\\u003e\\u0026\\u2028/\""].concat()
        );
    }

    #[test]
    fn blank_secrets_are_not_configured() {
        let parse = |value: Option<&str>| {
            deserialize_optional(serde_json::to_value(value).unwrap())
                .unwrap()
                .is_some()
        };
        assert!(!parse(None));
        assert!(!parse(Some("")));
        assert!(parse(Some("abc")));
    }
}
