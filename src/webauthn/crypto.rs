//! Public keys and the signatures attestation and sign-in check with them.
//!
//! RSA is done by hand on top of the modular exponentiation: OpenSSL takes
//! any key size and finds the RSA-PSS salt length itself (`salt_length:
//! :auto`), which webauthn-ruby relies on, and the rsa crate does neither.

use rsa::BigUint;
use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha384, Sha512};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => Sha1::digest(data).to_vec(),
            Self::Sha256 => Sha256::digest(data).to_vec(),
            Self::Sha384 => Sha384::digest(data).to_vec(),
            Self::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    fn len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }

    /// The DER `DigestInfo` prefix PKCS #1 v1.5 puts before the hash.
    fn digest_info_prefix(self) -> &'static [u8] {
        match self {
            Self::Sha1 => &[
                0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04,
                0x14,
            ],
            Self::Sha256 => &[
                0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x01, 0x05, 0x00, 0x04, 0x20,
            ],
            Self::Sha384 => &[
                0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x02, 0x05, 0x00, 0x04, 0x30,
            ],
            Self::Sha512 => &[
                0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x03, 0x05, 0x00, 0x04, 0x40,
            ],
        }
    }
}

/// The curves eunha can check signatures on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    P256,
    P384,
}

/// How an ECDSA signature arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcdsaEncoding {
    /// DER, as X.509 and OpenSSL's `verify` take it.
    Der,
    /// `r || s` when it is exactly twice the field size, DER otherwise, as
    /// openssl-signature_algorithm reads it for COSE and TPM signatures.
    RawOrDer,
    /// `r || s`, as JWS has it.
    Raw,
}

#[derive(Debug, Clone)]
pub enum PublicKey {
    P256(p256::ecdsa::VerifyingKey),
    P384(p384::ecdsa::VerifyingKey),
    Rsa {
        n: BigUint,
        e: BigUint,
    },
    /// A key eunha cannot check signatures with: another curve or type.
    Unsupported,
}

impl PartialEq for PublicKey {
    /// The same key, as comparing their `SubjectPublicKeyInfo` would find.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::P256(a), Self::P256(b)) => a == b,
            (Self::P384(a), Self::P384(b)) => a == b,
            (Self::Rsa { n: n1, e: e1 }, Self::Rsa { n: n2, e: e2 }) => n1 == n2 && e1 == e2,
            _ => false,
        }
    }
}

impl PublicKey {
    pub fn ec(curve: Curve, sec1: &[u8]) -> Option<Self> {
        match curve {
            Curve::P256 => p256::ecdsa::VerifyingKey::from_sec1_bytes(sec1)
                .ok()
                .map(Self::P256),
            Curve::P384 => p384::ecdsa::VerifyingKey::from_sec1_bytes(sec1)
                .ok()
                .map(Self::P384),
        }
    }

    pub fn rsa(n: &[u8], e: &[u8]) -> Self {
        Self::Rsa {
            n: BigUint::from_bytes_be(n),
            e: BigUint::from_bytes_be(e),
        }
    }

    pub fn curve(&self) -> Option<Curve> {
        match self {
            Self::P256(_) => Some(Curve::P256),
            Self::P384(_) => Some(Curve::P384),
            _ => None,
        }
    }

    pub fn is_ec(&self) -> bool {
        self.curve().is_some()
    }

    pub fn is_rsa(&self) -> bool {
        matches!(self, Self::Rsa { .. })
    }

    pub fn verify_ecdsa(
        &self,
        hash: Hash,
        message: &[u8],
        signature: &[u8],
        encoding: EcdsaEncoding,
    ) -> bool {
        use p256::ecdsa::signature::hazmat::PrehashVerifier as _;
        let digest = hash.digest(message);
        match self {
            Self::P256(key) => {
                let signature = if ecdsa_raw(signature, 32, encoding) {
                    p256::ecdsa::Signature::from_slice(signature)
                } else {
                    p256::ecdsa::Signature::from_der(signature)
                };
                signature.is_ok_and(|signature| key.verify_prehash(&digest, &signature).is_ok())
            }
            Self::P384(key) => {
                let signature = if ecdsa_raw(signature, 48, encoding) {
                    p384::ecdsa::Signature::from_slice(signature)
                } else {
                    p384::ecdsa::Signature::from_der(signature)
                };
                signature.is_ok_and(|signature| key.verify_prehash(&digest, &signature).is_ok())
            }
            _ => false,
        }
    }

    /// RSASSA-PKCS1-v1_5.
    pub fn verify_pkcs1(&self, hash: Hash, message: &[u8], signature: &[u8]) -> bool {
        let Some((em, k)) = self.rsa_open(signature) else {
            return false;
        };
        if signature.len() != k {
            return false;
        }
        let mut t = hash.digest_info_prefix().to_vec();
        t.extend(hash.digest(message));
        if k < t.len() + 11 {
            return false;
        }
        let mut expected = vec![0x00, 0x01];
        expected.resize(k - t.len() - 1, 0xff);
        expected.push(0x00);
        expected.extend(t);
        em == expected
    }

    /// RSASSA-PSS with MGF1 over the same hash, finding the salt length
    /// from the signature, as OpenSSL's `RSA_PSS_SALTLEN_AUTO` does.
    pub fn verify_pss(&self, hash: Hash, message: &[u8], signature: &[u8]) -> bool {
        let Self::Rsa { n, .. } = self else {
            return false;
        };
        let Some((em_full, k)) = self.rsa_open(signature) else {
            return false;
        };
        let mod_bits = n.bits();
        let em_bits = mod_bits - 1;
        let em_len = em_bits.div_ceil(8);
        // With a modulus a whole number of bytes long, EM is one byte
        // shorter than the key and its first byte must be zero.
        let em = if em_len < k {
            if em_full[0] != 0 {
                return false;
            }
            &em_full[1..]
        } else {
            &em_full[..]
        };
        let h_len = hash.len();
        if em_len < h_len + 2 || em[em_len - 1] != 0xbc {
            return false;
        }
        let (masked_db, rest) = em.split_at(em_len - h_len - 1);
        let h = &rest[..h_len];
        let unused_bits = 8 * em_len - em_bits;
        if unused_bits > 0 && masked_db[0] >> (8 - unused_bits) != 0 {
            return false;
        }
        let mut db = mgf1(hash, h, masked_db.len());
        for (d, m) in db.iter_mut().zip(masked_db) {
            *d ^= m;
        }
        if unused_bits > 0 {
            db[0] &= 0xff >> unused_bits;
        }
        let Some(one) = db.iter().position(|b| *b != 0) else {
            return false;
        };
        if db[one] != 0x01 {
            return false;
        }
        let salt = &db[one + 1..];
        let mut m_prime = vec![0u8; 8];
        m_prime.extend(hash.digest(message));
        m_prime.extend_from_slice(salt);
        hash.digest(&m_prime) == h
    }

    /// `s^e mod n` as a key-length string, and the key's length in bytes.
    fn rsa_open(&self, signature: &[u8]) -> Option<(Vec<u8>, usize)> {
        let Self::Rsa { n, e } = self else {
            return None;
        };
        let k = n.bits().div_ceil(8);
        if k == 0 || signature.len() > k {
            return None;
        }
        let s = BigUint::from_bytes_be(signature);
        if &s >= n {
            return None;
        }
        let m = s.modpow(e, n).to_bytes_be();
        if m.len() > k {
            return None;
        }
        let mut em = vec![0u8; k - m.len()];
        em.extend(m);
        Some((em, k))
    }
}

/// Whether `bytes` is to be read as `r || s` rather than DER.
fn ecdsa_raw(bytes: &[u8], field_len: usize, encoding: EcdsaEncoding) -> bool {
    match encoding {
        EcdsaEncoding::Der => false,
        EcdsaEncoding::RawOrDer => bytes.len() == 2 * field_len,
        EcdsaEncoding::Raw => true,
    }
}

fn mgf1(hash: Hash, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + hash.len());
    let mut counter = 0u32;
    while out.len() < len {
        let mut block = seed.to_vec();
        block.extend_from_slice(&counter.to_be_bytes());
        out.extend(hash.digest(&block));
        counter += 1;
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::traits::PublicKeyParts as _;

    fn rsa_pair() -> (rsa::RsaPrivateKey, PublicKey) {
        let private = rsa::RsaPrivateKey::new(&mut rand_08(), 1024).unwrap();
        let public = PublicKey::Rsa {
            n: private.n().clone(),
            e: private.e().clone(),
        };
        (private, public)
    }

    fn rand_08() -> impl rsa::rand_core::CryptoRngCore {
        rsa::rand_core::OsRng
    }

    #[test]
    fn checks_rsa_signatures_of_both_paddings() {
        use rsa::signature::{RandomizedSigner as _, SignatureEncoding as _, Signer as _};
        let (private, public) = rsa_pair();
        let message = b"attested";
        let pkcs1 = rsa::pkcs1v15::SigningKey::<Sha256>::new(private.clone())
            .sign(message)
            .to_vec();
        assert!(public.verify_pkcs1(Hash::Sha256, message, &pkcs1));
        assert!(!public.verify_pkcs1(Hash::Sha256, b"other", &pkcs1));
        assert!(!public.verify_pkcs1(Hash::Sha384, message, &pkcs1));

        // Any salt length is found, as OpenSSL's automatic one is.
        for salt in [0, 20, 32, 64] {
            let signer = rsa::pss::SigningKey::<Sha256>::new_with_salt_len(private.clone(), salt);
            let pss = signer.sign_with_rng(&mut rand_08(), message).to_vec();
            assert!(
                public.verify_pss(Hash::Sha256, message, &pss),
                "salt {salt}"
            );
            assert!(!public.verify_pss(Hash::Sha256, b"other", &pss));
            assert!(!public.verify_pkcs1(Hash::Sha256, message, &pss));
        }
    }

    #[test]
    fn reads_ecdsa_signatures_as_each_caller_sends_them() {
        use p256::ecdsa::{signature::Signer as _, Signature, SigningKey};
        let key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let public = PublicKey::P256(*key.verifying_key());
        let signature: Signature = key.sign(b"data");
        let der = signature.to_der();
        let raw = signature.to_bytes();
        assert!(public.verify_ecdsa(Hash::Sha256, b"data", der.as_bytes(), EcdsaEncoding::Der));
        assert!(!public.verify_ecdsa(Hash::Sha256, b"data", &raw, EcdsaEncoding::Der));
        assert!(public.verify_ecdsa(Hash::Sha256, b"data", &raw, EcdsaEncoding::RawOrDer));
        assert!(public.verify_ecdsa(
            Hash::Sha256,
            b"data",
            der.as_bytes(),
            EcdsaEncoding::RawOrDer
        ));
        assert!(public.verify_ecdsa(Hash::Sha256, b"data", &raw, EcdsaEncoding::Raw));
        assert!(!public.verify_ecdsa(Hash::Sha256, b"other", &raw, EcdsaEncoding::Raw));
    }
}
