//! Reads and writes the format Rails' `ActiveRecord::Encryption` stores.
//!
//! Mastodon 4.7.0 keeps local accounts' private keys in `keypairs.private_key`,
//! declared `encrypts :private_key`, so the column holds a Rails message
//! envelope rather than a PEM. Eunha has to speak that format in both
//! directions: to sign with keys written by a Mastodon it replaced, and to
//! leave keys a Mastodon could pick up if it were pointed back at the database.
//!
//! The scheme, as Mastodon configures it (`config/initializers/
//! active_record_encryption.rb`, `config.load_defaults 8.1`):
//!
//!   * The key is PBKDF2-HMAC over `ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY`,
//!     salted with `ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT`, 2^16
//!     iterations, 32 bytes out. Rails 7.1 moved the digest from SHA-1 to
//!     SHA-256; Mastodon sets `support_sha1_for_non_deterministic_encryption`,
//!     which keeps SHA-1 as a decryption fallback, so both are tried.
//!   * AES-256-GCM, random 12-byte IV, 16-byte tag, empty AAD.
//!   * Payloads over 140 bytes are zlib-deflated first and flagged `c`.
//!   * The result is JSON: `{"p":<base64>,"h":{"iv":<base64>,"at":<base64>}}`.
//!
//! Deterministic encryption is not implemented: `keypairs.private_key` is the
//! only encrypted column in Mastodon's schema, and it is not deterministic.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rand::TryRng;
use serde::{Deserialize, Serialize};

/// PBKDF2 iterations, from `ActiveSupport::KeyGenerator`'s default.
const ITERATIONS: u32 = 1 << 16;
/// AES-256 key length.
const KEY_LEN: usize = 32;
/// Rails will not compress below this, and says the threshold cannot change.
const COMPRESSION_THRESHOLD: usize = 140;

/// The JSON envelope Rails stores in an encrypted column.
#[derive(Serialize, Deserialize)]
struct Message {
    /// Ciphertext.
    p: String,
    #[serde(default)]
    h: Headers,
}

#[derive(Default, Serialize, Deserialize)]
struct Headers {
    iv: Option<String>,
    at: Option<String>,
    /// Set when the payload was compressed before encryption.
    #[serde(skip_serializing_if = "Option::is_none")]
    c: Option<bool>,
}

/// Encrypts and decrypts values for Rails-encrypted columns.
#[derive(Clone)]
pub struct Encryptor {
    primary_key: String,
    key_derivation_salt: String,
    /// Derived keys, newest scheme first. Decryption tries each in turn;
    /// encryption always uses the first.
    ///
    /// Derived on first use and shared across clones: PBKDF2 at Rails' 2^16
    /// iterations is deliberately expensive, and an instance that never reads
    /// an encrypted column should never pay for it.
    keys: std::sync::Arc<std::sync::OnceLock<Vec<[u8; KEY_LEN]>>>,
}

impl std::fmt::Debug for Encryptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the derived keys.
        f.debug_struct("Encryptor").finish_non_exhaustive()
    }
}

impl Encryptor {
    /// Take Mastodon's `ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY` and
    /// `ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT`. The keys themselves are
    /// derived when something is first encrypted or decrypted.
    pub fn new(primary_key: &str, key_derivation_salt: &str) -> Self {
        Self {
            primary_key: primary_key.to_string(),
            key_derivation_salt: key_derivation_salt.to_string(),
            keys: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// The derived keys: the SHA-256 scheme Rails 7.1 onwards writes with, and
    /// the SHA-1 one Mastodon keeps enabled to read what came before it.
    fn keys(&self) -> &[[u8; KEY_LEN]] {
        self.keys.get_or_init(|| {
            let mut sha256 = [0u8; KEY_LEN];
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
                self.primary_key.as_bytes(),
                self.key_derivation_salt.as_bytes(),
                ITERATIONS,
                &mut sha256,
            );

            let mut sha1 = [0u8; KEY_LEN];
            pbkdf2::pbkdf2_hmac::<sha1::Sha1>(
                self.primary_key.as_bytes(),
                self.key_derivation_salt.as_bytes(),
                ITERATIONS,
                &mut sha1,
            );

            vec![sha256, sha1]
        })
    }

    /// Encrypt `plaintext` into the envelope Rails expects.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let (payload, compressed) = if plaintext.len() > COMPRESSION_THRESHOLD {
            use flate2::write::ZlibEncoder;
            use std::io::Write;

            let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(plaintext.as_bytes())?;
            (encoder.finish()?, true)
        } else {
            (plaintext.as_bytes().to_vec(), false)
        };

        let mut iv = [0u8; 12];
        rand::rngs::SysRng
            .try_fill_bytes(&mut iv)
            .map_err(|e| anyhow!("could not generate an IV: {e}"))?;

        let key = self.keys().first().context("no encryption key")?;
        let cipher = Aes256Gcm::new(key.into());
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&iv),
                Payload {
                    msg: &payload,
                    aad: b"",
                },
            )
            .map_err(|_| anyhow!("AES-GCM encryption failed"))?;

        // Rails keeps the tag in its own header rather than appended to the
        // ciphertext, which is where the AEAD implementation leaves it.
        let split = sealed
            .len()
            .checked_sub(16)
            .context("ciphertext shorter than its auth tag")?;
        let (ciphertext, auth_tag) = sealed.split_at(split);

        Ok(serde_json::to_string(&Message {
            p: BASE64.encode(ciphertext),
            h: Headers {
                iv: Some(BASE64.encode(iv)),
                at: Some(BASE64.encode(auth_tag)),
                c: compressed.then_some(true),
            },
        })?)
    }

    /// Decrypt an envelope written by Rails or by [`Encryptor::encrypt`].
    pub fn decrypt(&self, serialized: &str) -> Result<String> {
        let message: Message =
            serde_json::from_str(serialized).context("not a Rails encrypted message")?;

        let ciphertext = BASE64.decode(&message.p).context("payload is not base64")?;
        let iv = BASE64
            .decode(message.h.iv.as_deref().context("message has no IV")?)
            .context("IV is not base64")?;
        let auth_tag = BASE64
            .decode(message.h.at.as_deref().context("message has no auth tag")?)
            .context("auth tag is not base64")?;
        if auth_tag.len() != 16 {
            bail!("auth tag is {} bytes, expected 16", auth_tag.len());
        }

        let mut sealed = ciphertext;
        sealed.extend_from_slice(&auth_tag);

        // Which scheme wrote this is not recorded, so try each derived key.
        let plaintext = self
            .keys()
            .iter()
            .find_map(|key| {
                Aes256Gcm::new(key.into())
                    .decrypt(
                        Nonce::from_slice(&iv),
                        Payload {
                            msg: &sealed,
                            aad: b"",
                        },
                    )
                    .ok()
            })
            .context("no configured key could decrypt the message")?;

        let plaintext = if message.h.c.unwrap_or(false) {
            use std::io::Read;

            let mut decoder = flate2::read::ZlibDecoder::new(&plaintext[..]);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .context("decrypted payload is not zlib data")?;
            out
        } else {
            plaintext
        };

        String::from_utf8(plaintext).context("decrypted payload is not UTF-8")
    }
}

/// Generate one of Mastodon's `ACTIVE_RECORD_ENCRYPTION_*` secrets.
///
/// Matches what `bin/rails db:encryption:init` produces — 32 characters of
/// `SecureRandom.alphanumeric` — so that secrets generated here and secrets
/// generated there are interchangeable. The value is used as a PBKDF2 password
/// and salt, not as key material directly, so its length is a matter of
/// entropy rather than of the cipher.
pub fn generate_secret() -> String {
    use rand::RngExt as _;

    rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encryptor() -> Encryptor {
        Encryptor::new(
            "a3d1b2c4e5f60718293a4b5c6d7e8f90",
            "1f2e3d4c5b6a798877665544332211ff",
        )
    }

    #[test]
    fn generates_secrets_rails_would_have_generated() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 32);
        assert!(
            secret.chars().all(|c| c.is_ascii_alphanumeric()),
            "SecureRandom.alphanumeric is [A-Za-z0-9] only, got {secret:?}"
        );
        assert_ne!(secret, generate_secret());
    }

    #[test]
    fn a_generated_pair_of_secrets_can_be_used_to_encrypt() {
        let encryptor = Encryptor::new(&generate_secret(), &generate_secret());
        let sealed = encryptor.encrypt("a private key").unwrap();
        assert_eq!(encryptor.decrypt(&sealed).unwrap(), "a private key");
    }

    #[test]
    fn round_trips_a_short_value() {
        let e = encryptor();
        let sealed = e.encrypt("hello").unwrap();
        // Short values are stored uncompressed, so no `c` header.
        assert!(!sealed.contains("\"c\""));
        assert_eq!(e.decrypt(&sealed).unwrap(), "hello");
    }

    #[test]
    fn round_trips_a_private_key_sized_value() {
        let e = encryptor();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQ".repeat(30)
        );
        let sealed = e.encrypt(&pem).unwrap();
        // Anything over 140 bytes is deflated first and flagged.
        assert!(sealed.contains("\"c\":true"));
        assert!(sealed.len() < pem.len(), "compression should pay off here");
        assert_eq!(e.decrypt(&sealed).unwrap(), pem);
    }

    #[test]
    fn envelope_has_the_shape_rails_writes() {
        let sealed = encryptor().encrypt("hello").unwrap();
        let value: serde_json::Value = serde_json::from_str(&sealed).unwrap();
        assert!(value.get("p").and_then(|p| p.as_str()).is_some());
        let iv = value["h"]["iv"].as_str().unwrap();
        let at = value["h"]["at"].as_str().unwrap();
        assert_eq!(BASE64.decode(iv).unwrap().len(), 12);
        assert_eq!(BASE64.decode(at).unwrap().len(), 16);
    }

    #[test]
    fn decrypts_a_message_written_with_the_sha1_scheme() {
        // Mastodon sets `support_sha1_for_non_deterministic_encryption`, so a
        // value written before Rails 7.1 must still decrypt. Simulate one by
        // encrypting with only the SHA-1 key available.
        let full = encryptor();
        let sha1_only = Encryptor {
            primary_key: String::new(),
            key_derivation_salt: String::new(),
            keys: std::sync::Arc::new(std::sync::OnceLock::from(vec![full.keys()[1]])),
        };
        let sealed = sha1_only.encrypt("legacy value").unwrap();
        assert_eq!(full.decrypt(&sealed).unwrap(), "legacy value");
    }

    #[test]
    fn rejects_a_message_from_another_instance() {
        let sealed = encryptor().encrypt("hello").unwrap();
        let other = Encryptor::new("different-primary-key", "different-salt");
        assert!(other.decrypt(&sealed).is_err());
    }

    #[test]
    fn rejects_a_truncated_auth_tag() {
        let e = encryptor();
        let sealed = e.encrypt("hello").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&sealed).unwrap();
        let at = value["h"]["at"].as_str().unwrap().to_string();
        let truncated = BASE64.decode(&at).unwrap()[..8].to_vec();
        value["h"]["at"] = serde_json::Value::String(BASE64.encode(truncated));
        assert!(e.decrypt(&value.to_string()).is_err());
    }
}
