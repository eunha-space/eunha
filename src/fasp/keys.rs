//! The Ed25519 keys a FASP registration exchanges, stored as Mastodon stores
//! them: this server's private key as PKCS#8 PEM (`private_to_pem`) in
//! `server_private_key_pem`, the provider's public key as SPKI PEM
//! (`public_to_pem`) in `provider_public_key_pem`. Both cross the wire as the
//! raw 32-byte key in base64, which is what `raw_public_key` and
//! `new_raw_public_key` read and write.

use anyhow::Context as _;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ojak::sig::integrity::PublicKey;
use sha2::{Digest as _, Sha256};

/// `OpenSSL::PKey.generate_key('ed25519').private_to_pem`.
pub fn generate_private_key_pem() -> anyhow::Result<String> {
    ojak::sig::integrity::generate_ed25519_key(&mut rsa::rand_core::OsRng)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The seed a PKCS#8 PEM private key signs with, and its public key.
pub fn parse_private_key_pem(pem: &str) -> anyhow::Result<([u8; 32], [u8; 32])> {
    ojak::sig::integrity::parse_ed25519_key(pem).map_err(|e| anyhow::anyhow!("{e}"))
}

/// `OpenSSL::PKey.new_raw_public_key('ed25519', raw).public_to_pem`.
pub fn public_key_to_pem(raw: &[u8; 32]) -> String {
    PublicKey::Ed25519(Box::new(*raw))
        .to_spki_pem()
        .expect("an Ed25519 key always encodes")
}

/// The raw key of an SPKI PEM Ed25519 public key (`raw_public_key`).
pub fn public_key_from_pem(pem: &str) -> anyhow::Result<[u8; 32]> {
    match PublicKey::from_spki_pem(pem).map_err(|e| anyhow::anyhow!("{e}"))? {
        PublicKey::Ed25519(raw) => Ok(*raw),
        PublicKey::MlDsa44(_) => anyhow::bail!("not an Ed25519 public key"),
    }
}

/// `Base64.strict_decode64` of a raw public key, refused unless it is one.
pub fn public_key_from_base64(value: &str) -> anyhow::Result<[u8; 32]> {
    let raw = BASE64
        .decode(value.as_bytes())
        .context("the public key is not base64")?;
    raw.as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("an Ed25519 public key of {} bytes", raw.len()))
}

/// `OpenSSL::Digest.base64digest('sha256', raw)`: what the registration
/// page shows an administrator to compare with the provider's own.
pub fn fingerprint(raw: &[u8; 32]) -> String {
    BASE64.encode(Sha256::digest(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_keys_round_trip_through_pem() {
        let pem = generate_private_key_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        let (_, public) = parse_private_key_pem(&pem).unwrap();
        let spki = public_key_to_pem(&public);
        assert!(spki.starts_with("-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA"));
        assert_eq!(public_key_from_pem(&spki).unwrap(), public);
    }

    /// RFC 8410 §10.1's example public key.
    #[test]
    fn reads_the_rfc_8410_public_key() {
        let pem = "-----BEGIN PUBLIC KEY-----\n\
                   MCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n\
                   -----END PUBLIC KEY-----\n";
        let raw = public_key_from_pem(pem).unwrap();
        assert_eq!(
            hex::encode(raw),
            "19bf44096984cdfe8541bac167dc3b96c85086aa30b6b6cb0c5c38ad703166e1"
        );
        assert_eq!(public_key_to_pem(&raw), pem);
    }
}
