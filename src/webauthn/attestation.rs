//! Attestation statements, verified as webauthn-ruby 3.4.3 verifies them
//! under Mastodon's configuration.
//!
//! Mastodon leaves webauthn-ruby's defaults alone: the statement is
//! verified (`verify_attestation_statement`), every attestation type is
//! acceptable, the algorithms are ES256, PS256 and RS256, and no
//! `attestation_root_certificates_finders` are configured. So each format
//! falls back on the roots its gem ships, and a format whose gem ships none
//! refuses every statement that needs a chain:
//!
//!  -  `none`: the statement must be empty.
//!  -  `packed`: self attestation is checked against the new key; with an
//!     `x5c` chain, the certificate requirements, AAGUID and signature are
//!     checked and then the chain is refused, as no roots are configured.
//!  -  `fido-u2f`: likewise checked, then refused for want of roots.
//!  -  `android-key`: checked against the Google hardware attestation root
//!     android_key_attestation 0.3.0 ships.
//!  -  `android-safetynet`: checked against the six Google roots
//!     safety_net_attestation 0.5.0 ships.
//!  -  `tpm`: checked against the TPM vendor roots tpm-key_attestation
//!     0.14.1 ships.
//!  -  `apple`: checked against the Apple WebAuthn root webauthn-ruby ships.
//!
//! Any other format is refused.

use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sha2::{Digest as _, Sha256};

use super::cbor::Value;
use super::crypto::{Curve, EcdsaEncoding, Hash, PublicKey};
use super::der::{self, Tlv};
use super::x509::{self, oids, Certificate};
use super::{invalid, CoseKey, Error, Result};

/// The algorithms Mastodon's relying party accepts
/// (`WebAuthn::RelyingParty::DEFAULT_ALGORITHMS`).
const RP_ALGORITHMS: &[&str] = &["ES256", "PS256", "RS256"];

/// Every algorithm cose-ruby and webauthn-ruby register, by id and name.
const COSE_ALGORITHMS: &[(i64, &str)] = &[
    (-7, "ES256"),
    (-35, "ES384"),
    (-36, "ES512"),
    (-47, "ES256K"),
    (-8, "EdDSA"),
    (-37, "PS256"),
    (-38, "PS384"),
    (-39, "PS512"),
    (4, "HMAC 256/64"),
    (5, "HMAC 256/256"),
    (6, "HMAC 384/384"),
    (7, "HMAC 512/512"),
    (-257, "RS256"),
    (-258, "RS384"),
    (-259, "RS512"),
    (-65535, "RS1"),
];

const AAGUID_EXTENSION: &str = "1.3.6.1.4.1.45724.1.1.4";
const ANDROID_KEY_DESCRIPTION: &str = "1.3.6.1.4.1.11129.2.1.17";
const APPLE_NONCE: &str = "1.2.840.113635.100.8.2";
const TCG_KP_AIK_CERTIFICATE: &str = "2.23.133.8.3";
const TCG_AT_TPM_MANUFACTURER: &str = "2.23.133.2.1";
const TCG_AT_TPM_MODEL: &str = "2.23.133.2.2";
const TCG_AT_TPM_VERSION: &str = "2.23.133.2.3";

/// `TPM::VENDOR_IDS`.
const TPM_VENDOR_IDS: &[&str] = &[
    "id:414D4400",
    "id:41544D4C",
    "id:4252434D",
    "id:49424D00",
    "id:49465800",
    "id:494E5443",
    "id:4C454E00",
    "id:4E534D20",
    "id:4E545A00",
    "id:4E544300",
    "id:51434F4D",
    "id:534D5343",
    "id:53544D20",
    "id:534D534E",
    "id:534E5300",
    "id:54584E00",
    "id:57454300",
    "id:524F4343",
];

macro_rules! roots {
    ($($path:literal),* $(,)?) => {
        &[$(include_bytes!(concat!("roots/", $path)).as_slice()),*]
    };
}

/// `TPM::KeyAttestation::TRUSTED_CERTIFICATES`.
const TPM_ROOTS: &[&[u8]] = roots![
    "tpm/AMD-AMD-fTPM-ECC-RootCA.der",
    "tpm/AMD-AMD-fTPM-RSA-RootCA.der",
    "tpm/Atmel-Atmel-TPM-Root-Signing-Module.der",
    "tpm/Infineon-IFX-RootCA.der",
    "tpm/Infineon-IFX-TPM-EK-Root-CA.der",
    "tpm/Infineon-Infineon-OPTIGA-TM-ECC-Root-CA.der",
    "tpm/Infineon-Infineon-OPTIGA-TM-RSA-Root-CA.der",
    "tpm/Intel-EKRootPublicKey.der",
    "tpm/Microsoft-Microsoft-TPM-Root-Certificate-Authority-2014.der",
    "tpm/NationZ-EkRootCA.der",
    "tpm/Nuvoton-NTC-TPM-EK-Root-CA-01.der",
    "tpm/Nuvoton-NTC-TPM-EK-Root-CA-02.der",
    "tpm/Nuvoton-NTC-TPM-EK-Root-CA-ARSUF-01.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-1013.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-1014.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-1110.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-1111.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-2010.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-2011.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-2110.der",
    "tpm/Nuvoton-Nuvoton-TPM-Root-CA-2111.der",
    "tpm/STMicro-GlobalSign-Trusted-Computing-CA.der",
    "tpm/STMicro-GlobalSign-Trusted-Platform-Module-ECC-Root-CA.der",
    "tpm/STMicro-ST-TPM-Root-Certificate.der",
    "tpm/STMicro-STM-TPM-ECC-Root-CA-01.der",
];

/// `SafetyNetAttestation::Statement::GOOGLE_ROOT_CERTIFICATES`.
const SAFETYNET_ROOTS: &[&[u8]] = roots![
    "safetynet/GSR2.der",
    "safetynet/GSR4.der",
    "safetynet/GTSR1.der",
    "safetynet/GTSR2.der",
    "safetynet/GTSR3.der",
    "safetynet/GTSR4.der",
];

/// `AndroidKeyAttestation::Statement::GOOGLE_ROOT_CERTIFICATES`.
const ANDROID_KEY_ROOTS: &[&[u8]] = roots!["android_key/google-hardware-attestation-root.der"];

/// `WebAuthn::AttestationStatement::Apple::ROOT_CERTIFICATE`.
const APPLE_ROOTS: &[&[u8]] = roots!["apple/apple-webauthn-root-ca.der"];

/// What attestation is checked against: the time, and the roots.
pub struct Trust {
    pub now: DateTime<Utc>,
    /// Roots that replace every format's own, as a configured
    /// `attestation_root_certificates_finders` would. Mastodon configures
    /// none.
    pub roots: Option<Vec<Vec<u8>>>,
}

impl Trust {
    /// Mastodon's configuration, now.
    pub fn mastodon() -> Self {
        Self {
            now: Utc::now(),
            roots: None,
        }
    }

    /// `root_certificates`: the configured roots, else the format's own.
    fn roots(&self, format: &str) -> Result<Vec<Certificate>> {
        if let Some(roots) = self.roots.as_ref().filter(|r| !r.is_empty()) {
            return x509::parse_all(roots);
        }
        let defaults: &[&[u8]] = match format {
            "android-key" => ANDROID_KEY_ROOTS,
            "android-safetynet" => SAFETYNET_ROOTS,
            "tpm" => TPM_ROOTS,
            "apple" => APPLE_ROOTS,
            _ => &[],
        };
        defaults.iter().map(|d| Certificate::from_der(d)).collect()
    }
}

/// What the statement is checked against.
pub struct Attested<'a> {
    /// The authenticator data, as signed.
    pub auth_data: &'a [u8],
    pub rp_id_hash: &'a [u8],
    pub aaguid: &'a [u8],
    pub credential_id: &'a [u8],
    pub credential_key: &'a CoseKey,
    pub client_data_hash: &'a [u8],
}

impl Attested<'_> {
    /// `authenticator_data.data + client_data_hash`.
    fn signed_data(&self) -> Vec<u8> {
        let mut data = self.auth_data.to_vec();
        data.extend_from_slice(self.client_data_hash);
        data
    }
}

/// `WebAuthn::AttestationStatement.from(fmt, statement).valid?(...)`.
pub fn verify(
    format: &str,
    statement: &Value,
    attested: &Attested<'_>,
    trust: &Trust,
) -> Result<()> {
    match format {
        "none" => none(statement),
        "packed" => packed(statement, attested, trust),
        "fido-u2f" => fido_u2f(statement, attested, trust),
        "android-key" => android_key(statement, attested, trust),
        "android-safetynet" => android_safetynet(statement, attested, trust),
        "tpm" => tpm(statement, attested, trust),
        "apple" => apple(statement, attested, trust),
        _ => invalid("unsupported attestation format"),
    }
}

fn check(ok: bool, why: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        invalid(why)
    }
}

fn none(statement: &Value) -> Result<()> {
    check(
        matches!(statement, Value::Map(entries) if entries.is_empty()),
        "none attestation has a statement",
    )
}

/// `statement["x5c"]` parsed, or `None` when there is none.
fn certificates(statement: &Value) -> Result<Option<Vec<Certificate>>> {
    let Some(x5c) = statement.get_text("x5c") else {
        return Ok(None);
    };
    let Value::Array(items) = x5c else {
        return invalid("x5c is not an array");
    };
    let ders = items
        .iter()
        .map(|item| {
            item.as_bytes().map(<[u8]>::to_vec).ok_or(Error::Invalid(
                "x5c holds something other than a certificate",
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    x509::parse_all(&ders).map(Some)
}

fn signature(statement: &Value) -> Option<&[u8]> {
    statement.get_text("sig").and_then(Value::as_bytes)
}

/// `COSE::Algorithm.find(statement["alg"])`, which takes an id or a name.
fn find_algorithm(value: Option<&Value>) -> Option<&'static str> {
    match value? {
        Value::Int(id) => COSE_ALGORITHMS.iter().find(|(i, _)| i == id),
        Value::Text(name) => COSE_ALGORITHMS.iter().find(|(_, n)| n == name),
        _ => None,
    }
    .map(|(_, name)| *name)
}

/// `Base#valid_signature?`: the statement's algorithm must be one the
/// relying party accepts and suit the key, and the signature must hold.
fn valid_signature(statement: &Value, key: &PublicKey, data: &[u8]) -> Result<()> {
    let alg = find_algorithm(statement.get_text("alg"))
        .filter(|name| RP_ALGORITHMS.contains(name))
        .ok_or(Error::Invalid("unsupported attestation algorithm"))?;
    let signature = signature(statement).ok_or(Error::Invalid("attestation has no signature"))?;
    check(
        verify_cose(alg, key, data, signature)?,
        "attestation signature does not verify",
    )
}

/// A signature by one of the relying party's algorithms, as cose-ruby
/// checks it; a key that does not suit the algorithm is an error.
pub fn verify_cose(alg: &str, key: &PublicKey, data: &[u8], signature: &[u8]) -> Result<bool> {
    match alg {
        "ES256" if key.is_ec() => {
            check(key.curve() == Some(Curve::P256), "key is not on P-256")?;
            Ok(key.verify_ecdsa(Hash::Sha256, data, signature, EcdsaEncoding::RawOrDer))
        }
        "PS256" if key.is_rsa() => Ok(key.verify_pss(Hash::Sha256, data, signature)),
        "RS256" if key.is_rsa() => Ok(key.verify_pkcs1(Hash::Sha256, data, signature)),
        _ => invalid("incompatible algorithm and key"),
    }
}

/// `Base#matching_aaguid?`: the certificate's AAGUID extension, if any,
/// names the authenticator's AAGUID.
fn matching_aaguid(certificate: Option<&Certificate>, aaguid: &[u8]) -> Result<()> {
    let Some(extension) = certificate.and_then(|c| c.extension(AAGUID_EXTENSION)) else {
        return Ok(());
    };
    let value = Tlv::parse(&extension.value)?;
    check(value.value == aaguid, "certificate AAGUID does not match")
}

/// `Base#valid_certificate_chain?`, for the formats that use it.
fn valid_certificate_chain(
    format: &str,
    certificates: &[Certificate],
    trust: &Trust,
) -> Result<()> {
    let roots = trust.roots(format)?;
    let leaf = certificates
        .first()
        .ok_or(Error::Invalid("no attestation certificate"))?;
    if certificates.len() == 1 && roots.iter().any(|r| r.der == leaf.der) {
        return Ok(());
    }
    x509::verify_chain(leaf, certificates, &roots, trust.now).map(|_| ())
}

fn packed(statement: &Value, attested: &Attested<'_>, trust: &Trust) -> Result<()> {
    let alg = statement.get_text("alg");
    check(
        is_truthy(alg) && is_truthy(statement.get_text("sig")),
        "packed attestation has no algorithm or signature",
    )?;
    let certificates = certificates(statement)?;
    if certificates.is_none() && alg != Some(&Value::Int(attested.credential_key.alg)) {
        return invalid("attestation algorithm does not match the key");
    }
    if let Some(leaf) = certificates.as_ref().and_then(|c| c.first()) {
        // The packed attestation certificate requirements.
        let ou = leaf.subject.first(oids::ORGANIZATIONAL_UNIT);
        check(
            leaf.version == 2
                && ou == Some(b"Authenticator Attestation".as_slice())
                && leaf.basic_constraints_ca_false(),
            "attestation certificate does not meet the requirements",
        )?;
    }
    let leaf = certificates.as_ref().and_then(|c| c.first());
    matching_aaguid(leaf, attested.aaguid)?;
    let key = leaf.map_or(&attested.credential_key.key, |c| &c.public_key);
    valid_signature(statement, key, &attested.signed_data())?;
    match &certificates {
        // Self attestation needs no chain.
        None => Ok(()),
        Some(certificates) => valid_certificate_chain("packed", certificates, trust),
    }
}

/// Ruby truthiness: anything but nil and false.
fn is_truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

fn fido_u2f(statement: &Value, attested: &Attested<'_>, trust: &Trust) -> Result<()> {
    let certificates = certificates(statement)?;
    let certificates = match certificates {
        Some(c) if c.len() == 1 && is_truthy(statement.get_text("sig")) => c,
        _ => return invalid("fido-u2f attestation needs one certificate and a signature"),
    };
    let leaf = &certificates[0];
    check(
        leaf.public_key.curve() == Some(Curve::P256),
        "fido-u2f certificate key is not on P-256",
    )?;
    let (x, y) = attested
        .credential_key
        .ec_coordinates()
        .filter(|(x, y)| x.len() == 32 && y.len() == 32)
        .filter(|_| attested.credential_key.alg == -7)
        .ok_or(Error::Invalid("fido-u2f credential key is not a P-256 key"))?;
    check(attested.aaguid == [0u8; 16], "fido-u2f AAGUID is not zero")?;

    let mut data = vec![0x00];
    data.extend_from_slice(attested.rp_id_hash);
    data.extend_from_slice(attested.client_data_hash);
    data.extend_from_slice(attested.credential_id);
    data.push(0x04);
    data.extend_from_slice(x);
    data.extend_from_slice(y);
    let signature = signature(statement).ok_or(Error::Invalid("attestation has no signature"))?;
    check(
        verify_cose("ES256", &leaf.public_key, &data, signature)?,
        "attestation signature does not verify",
    )?;
    valid_certificate_chain("fido-u2f", &certificates, trust)
}

fn android_key(statement: &Value, attested: &Attested<'_>, trust: &Trust) -> Result<()> {
    let certificates =
        certificates(statement)?.ok_or(Error::Invalid("android-key attestation has no x5c"))?;
    let leaf = certificates
        .first()
        .ok_or(Error::Invalid("no attestation certificate"))?;
    valid_signature(statement, &leaf.public_key, &attested.signed_data())?;
    check(
        leaf.public_key == attested.credential_key.key,
        "attestation certificate is not for the new key",
    )?;

    let extension = leaf
        .extension(ANDROID_KEY_DESCRIPTION)
        .ok_or(Error::Invalid("no key description"))?;
    let description = Tlv::parse(&extension.value)?.children()?;
    let challenge = description
        .get(4)
        .filter(|c| !c.constructed)
        .ok_or(Error::Invalid("key description has no challenge"))?;
    check(
        challenge.value == attested.client_data_hash,
        "attestation challenge does not match",
    )?;
    let software = AuthorizationList(description.get(6).copied());
    let tee = AuthorizationList(description.get(7).copied());

    check(
        !tee.all_applications()? && !software.all_applications()?,
        "key is for all applications",
    )?;
    check(
        tee.origin()? == Some(ORIGIN_GENERATED) || software.origin()? == Some(ORIGIN_GENERATED),
        "key was not generated on the device",
    )?;
    check(
        tee.purpose()?.as_deref() == Some(&[PURPOSE_SIGN])
            || software.purpose()?.as_deref() == Some(&[PURPOSE_SIGN]),
        "key is not for signing alone",
    )?;

    let roots = trust.roots("android-key")?;
    x509::verify_chain(leaf, &certificates[1..], &roots, trust.now).map(|_| ())
}

const PURPOSE_SIGN: u64 = 2;
const ORIGIN_GENERATED: u64 = 0;

/// An Android KeyMaster `AuthorizationList`, read as
/// android_key_attestation reads it: lazily, so a field it never looks at
/// cannot refuse the key.
struct AuthorizationList<'a>(Option<Tlv<'a>>);

impl<'a> AuthorizationList<'a> {
    fn find(&self, tag: u32) -> Result<Option<Tlv<'a>>> {
        let list = self
            .0
            .filter(|l| l.constructed)
            .ok_or(Error::Invalid("malformed authorization list"))?;
        Ok(list.children()?.into_iter().find(|c| c.tag == tag))
    }

    /// The first value inside an explicitly tagged field.
    fn inner(field: Tlv<'a>) -> Result<Option<Tlv<'a>>> {
        if !field.constructed {
            return invalid("malformed authorization list entry");
        }
        Ok(field.children()?.first().copied())
    }

    fn all_applications(&self) -> Result<bool> {
        Ok(self.find(600)?.is_some())
    }

    fn origin(&self) -> Result<Option<u64>> {
        let Some(field) = self.find(702)? else {
            return Ok(None);
        };
        let Some(value) = Self::inner(field)? else {
            return Ok(None);
        };
        // `ORIGIN_ENUM.fetch`: generated, derived, imported, unknown.
        let origin = value.small_uint()?;
        check(origin <= 3, "unknown key origin")?;
        Ok(Some(origin))
    }

    fn purpose(&self) -> Result<Option<Vec<u64>>> {
        let Some(field) = self.find(1)? else {
            return Ok(None);
        };
        let Some(set) = Self::inner(field)? else {
            return Ok(None);
        };
        let purposes = set
            .children()?
            .iter()
            .map(Tlv::small_uint)
            .collect::<Result<Vec<_>>>()?;
        // `PURPOSE_ENUM.fetch`: encrypt to wrap_key.
        check(purposes.iter().all(|p| *p <= 5), "unknown key purpose")?;
        Ok(Some(purposes))
    }
}

fn android_safetynet(statement: &Value, attested: &Attested<'_>, trust: &Trust) -> Result<()> {
    let nonce =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(attested.signed_data()));
    let response = statement
        .get_text("response")
        .and_then(Value::as_bytes)
        .ok_or(Error::Invalid(
            "android-safetynet attestation has no response",
        ))?;
    let roots = trust.roots("android-safetynet")?;
    let payload = safetynet_response(response, &nonce, &roots, trust.now)?;

    // `valid_version?`
    check(
        statement
            .get_text("ver")
            .and_then(Value::as_text)
            .is_some_and(|v| !v.is_empty()),
        "android-safetynet attestation has no version",
    )?;
    // `cts_profile_match?`
    check(
        !matches!(
            payload.get("ctsProfileMatch"),
            None | Some(Json::Null | Json::Bool(false))
        ),
        "device does not match a compatible profile",
    )
}

/// `SafetyNetAttestation::Statement#verify(nonce, trusted_certificates:,
/// time:)`, with `JWT.decode` underneath: the payload when it holds.
fn safetynet_response(
    jws: &[u8],
    nonce: &str,
    roots: &[Certificate],
    now: DateTime<Utc>,
) -> Result<serde_json::Map<String, Json>> {
    let jws = std::str::from_utf8(jws).or_else(|_| invalid("response is not a JWS"))?;
    let parts: Vec<&str> = jws.split('.').collect();
    let [header_b64, payload_b64, signature_b64] = parts.as_slice() else {
        return invalid("response is not a JWS");
    };
    let b64url = |s: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim_end_matches('='))
            .or_else(|_| invalid("JWS is not base64url"))
    };
    let header: Json = serde_json::from_slice(&b64url(header_b64)?)
        .or_else(|_| invalid("JWS header is not JSON"))?;
    let payload: Json = serde_json::from_slice(&b64url(payload_b64)?)
        .or_else(|_| invalid("JWS payload is not JSON"))?;
    let Json::Object(payload) = payload else {
        return invalid("JWS payload is not an object");
    };
    let signature = b64url(signature_b64)?;
    let alg = header.get("alg").and_then(Json::as_str);
    check(
        matches!(alg, Some("ES256" | "RS256")),
        "JWS algorithm is not allowed",
    )?;

    // `X5cKeyFinder.from`
    let x5c = header
        .get("x5c")
        .and_then(Json::as_array)
        .ok_or(Error::Invalid("JWS has no x5c"))?;
    let ders = x5c
        .iter()
        .map(|c| {
            c.as_str()
                .and_then(|c| base64::engine::general_purpose::STANDARD.decode(c).ok())
                .ok_or(Error::Invalid("x5c entry is not base64"))
        })
        .collect::<Result<Vec<_>>>()?;
    let certificates = x509::parse_all(&ders)?;
    let (leaf, rest) = certificates
        .split_first()
        .ok_or(Error::Invalid("JWS x5c is empty"))?;
    let chain = x509::verify_chain(leaf, rest, roots, now)?;
    let key = &chain[0].public_key;

    let signing_input = format!("{header_b64}.{payload_b64}");
    let verified = match alg {
        Some("ES256") => {
            check(
                key.curve() == Some(Curve::P256),
                "JWS key does not suit ES256",
            )?;
            key.verify_ecdsa(
                Hash::Sha256,
                signing_input.as_bytes(),
                &signature,
                EcdsaEncoding::Raw,
            )
        }
        _ => {
            check(key.is_rsa(), "JWS key does not suit RS256")?;
            key.verify_pkcs1(Hash::Sha256, signing_input.as_bytes(), &signature)
        }
    };
    check(verified, "JWS signature does not verify")?;
    // JWT's default claim checks, with no leeway.
    let seconds = now.timestamp();
    if let Some(exp) = payload.get("exp") {
        check(json_to_i(exp) > seconds, "JWS has expired")?;
    }
    if let Some(nbf) = payload.get("nbf") {
        check(json_to_i(nbf) <= seconds, "JWS is not yet valid")?;
    }

    check(
        chain[0].subject.first(oids::COMMON_NAME) == Some(b"attest.android.com".as_slice()),
        "JWS certificate is not for attest.android.com",
    )?;
    check(
        payload.get("nonce").and_then(Json::as_str) == Some(nonce),
        "attestation nonce does not match",
    )?;
    let timestamp = payload
        .get("timestampMs")
        .and_then(Json::as_f64)
        .ok_or(Error::Invalid("attestation has no timestamp"))?
        / 1000.0;
    let now = now.timestamp_micros() as f64 / 1_000_000.0;
    check(
        (now - 60.0..=now + 60.0).contains(&timestamp),
        "attestation timestamp is not within a minute",
    )?;
    Ok(payload)
}

/// Ruby's `to_i` on a JSON value.
fn json_to_i(value: &Json) -> i64 {
    match value {
        Json::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Json::String(s) => s
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect::<String>()
            .parse()
            .unwrap_or(0),
        _ => 0,
    }
}

fn apple(statement: &Value, attested: &Attested<'_>, trust: &Trust) -> Result<()> {
    let certificates = certificates(statement)?;
    let leaf = certificates
        .as_ref()
        .and_then(|c| c.first())
        .ok_or(Error::Invalid("apple attestation has no certificate"))?;
    let extension = leaf
        .extension(APPLE_NONCE)
        .ok_or(Error::Invalid("apple attestation has no nonce"))?;
    let sequence = Tlv::parse(&extension.value)?;
    let expected = Sha256::digest(attested.signed_data());
    let nonce = (sequence.tag == der::SEQUENCE)
        .then(|| sequence.children().ok())
        .flatten()
        .filter(|items| items.len() == 1)
        .and_then(|items| items[0].children().ok())
        .and_then(|inner| inner.first().copied())
        .filter(|octets| !octets.constructed);
    check(
        nonce.is_some_and(|n| n.value == expected.as_slice()),
        "apple attestation nonce does not match",
    )?;
    check(
        leaf.public_key == attested.credential_key.key,
        "attestation certificate is not for the new key",
    )?;
    valid_certificate_chain("apple", certificates.as_deref().unwrap_or_default(), trust)
}

fn tpm(statement: &Value, attested: &Attested<'_>, trust: &Trust) -> Result<()> {
    let certificates =
        certificates(statement)?.ok_or(Error::Invalid("tpm attestation has no x5c"))?;
    check(
        statement.get_text("ver").and_then(Value::as_text) == Some("2.0"),
        "tpm attestation is not version 2.0",
    )?;
    // The TPM's own algorithm table, which takes any algorithm cose-ruby
    // knows rather than only the relying party's.
    let alg = find_algorithm(statement.get_text("alg"))
        .ok_or(Error::Invalid("unsupported attestation algorithm"))?;
    let (scheme, hash) = match alg {
        "RS1" => (TpmScheme::RsaSsa, Hash::Sha1),
        "RS256" => (TpmScheme::RsaSsa, Hash::Sha256),
        "PS256" => (TpmScheme::RsaPss, Hash::Sha256),
        "ES256" => (TpmScheme::Ecdsa, Hash::Sha256),
        _ => return invalid("unsupported tpm algorithm"),
    };
    let qualifying_data = hash.digest(&attested.signed_data());
    let cert_info = statement
        .get_text("certInfo")
        .and_then(Value::as_bytes)
        .ok_or(Error::Invalid("tpm attestation has no certInfo"))?;
    let pub_area = statement
        .get_text("pubArea")
        .and_then(Value::as_bytes)
        .ok_or(Error::Invalid("tpm attestation has no pubArea"))?;
    let signature = signature(statement).ok_or(Error::Invalid("attestation has no signature"))?;
    let aik = certificates
        .first()
        .ok_or(Error::Invalid("no attestation certificate"))?;

    // `TPM::CertifyValidator#valid?`
    let attest = tpm::Attest::parse(cert_info)?;
    let public = tpm::Public::parse(pub_area)?;
    check(
        attest.attested_type == tpm::ST_ATTEST_CERTIFY
            && attest.extra_data == qualifying_data
            && attest.magic == tpm::GENERATED_VALUE,
        "tpm certInfo does not certify this attestation",
    )?;
    check(
        attest.name == public.name(pub_area)?,
        "tpm certInfo does not name the pubArea",
    )?;
    let verified = match scheme {
        TpmScheme::RsaSsa => {
            aik.public_key.is_rsa() && aik.public_key.verify_pkcs1(hash, cert_info, signature)
        }
        TpmScheme::RsaPss => {
            aik.public_key.is_rsa() && aik.public_key.verify_pss(hash, cert_info, signature)
        }
        TpmScheme::Ecdsa => {
            aik.public_key.curve() == Some(Curve::P256)
                && aik
                    .public_key
                    .verify_ecdsa(hash, cert_info, signature, EcdsaEncoding::RawOrDer)
        }
    };
    check(verified, "tpm signature does not verify")?;

    // `TPM::AIKCertificate#conformant?`
    check(
        aik_conformant(aik, trust.now),
        "tpm AIK certificate does not conform",
    )?;

    // `TPM::KeyAttestation#trustworthy?`
    let mut roots: Vec<Certificate> = Vec::new();
    for root in trust.roots("tpm")? {
        if !roots.iter().any(|r| r.serial == root.serial) {
            roots.push(root);
        }
    }
    x509::verify_chain(aik, &certificates[1..], &roots, trust.now)?;

    check(
        public
            .key()?
            .is_some_and(|key| key == attested.credential_key.key),
        "tpm pubArea is not the new key",
    )?;
    matching_aaguid(Some(aik), attested.aaguid)
}

enum TpmScheme {
    RsaSsa,
    RsaPss,
    Ecdsa,
}

fn aik_conformant(aik: &Certificate, now: DateTime<Utc>) -> bool {
    let in_use = aik.not_before < now && now < aik.not_after;
    let eku = aik.extension(oids::EXT_KEY_USAGE).is_some_and(|ext| {
        !ext.critical
            && Tlv::parse(&ext.value)
                .and_then(|t| t.sequence())
                .is_ok_and(|purposes| {
                    purposes.len() == 1
                        && purposes[0].oid().ok()
                            == Some(der::oid(TCG_KP_AIK_CERTIFICATE).as_slice())
                })
    });
    let basic_constraints = aik.basic_constraints_ca_false()
        && aik
            .extension(oids::BASIC_CONSTRAINTS)
            .is_some_and(|e| e.critical);
    in_use
        && aik.version == 2
        && eku
        && basic_constraints
        && aik.subject.is_empty()
        && aik_san_valid(aik).unwrap_or(false)
}

/// The subject alternative name must be critical and hold a directory name
/// with the TPM's manufacturer, a known one, its model and version.
fn aik_san_valid(aik: &Certificate) -> Result<bool> {
    let Some(san) = aik.extension(oids::SUBJECT_ALT_NAME) else {
        return Ok(false);
    };
    if !san.critical {
        return Ok(false);
    }
    let names = Tlv::parse(&san.value)?.children()?;
    let directory = names
        .iter()
        .find(|n| n.is(der::CLASS_CONTEXT, 4))
        .ok_or(Error::Invalid("no directory name"))?;
    let name = directory
        .children()?
        .first()
        .map(x509::Name::parse)
        .ok_or(Error::Invalid("empty directory name"))??;
    let get = |oid| {
        name.first(oid)
            .ok_or(Error::Invalid("TPM attribute missing"))
    };
    let manufacturer = get(TCG_AT_TPM_MANUFACTURER)?;
    let model = get(TCG_AT_TPM_MODEL)?;
    let version = get(TCG_AT_TPM_VERSION)?;
    Ok(!manufacturer.is_empty()
        && TPM_VENDOR_IDS
            .iter()
            .any(|id| id.as_bytes() == manufacturer)
        && !model.is_empty()
        && !version.is_empty())
}

/// The TPM 2.0 structures a `tpm` statement carries, read as
/// tpm-key_attestation's BinData records read them.
mod tpm {
    use super::super::{invalid, Error, Result};
    use super::{Curve, Hash, PublicKey};

    pub const GENERATED_VALUE: u32 = 0xFF54_4347;
    pub const ST_ATTEST_CERTIFY: u16 = 0x8017;

    const ALG_RSA: u16 = 0x0001;
    const ALG_SHA1: u16 = 0x0004;
    const ALG_SHA256: u16 = 0x000B;
    const ALG_NULL: u16 = 0x0010;
    const ALG_RSASSA: u16 = 0x0014;
    const ALG_RSAPSS: u16 = 0x0016;
    const ALG_ECDSA: u16 = 0x0018;
    const ALG_ECC: u16 = 0x0023;

    const ECC_NIST_P256: u16 = 0x0003;
    const ECC_NIST_P384: u16 = 0x0004;
    const ECC_NIST_P521: u16 = 0x0005;

    struct Reader<'a>(&'a [u8]);

    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            if self.0.len() < n {
                return invalid("tpm structure ends early");
            }
            let (head, rest) = self.0.split_at(n);
            self.0 = rest;
            Ok(head)
        }

        fn u16(&mut self) -> Result<u16> {
            let b = self.take(2)?;
            Ok(u16::from_be_bytes([b[0], b[1]]))
        }

        fn u32(&mut self) -> Result<u32> {
            let b = self.take(4)?;
            Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        }

        fn sized(&mut self) -> Result<&'a [u8]> {
            let n = self.u16()?;
            self.take(usize::from(n))
        }
    }

    /// `TPMS_ATTEST` carrying `TPMS_CERTIFY_INFO`.
    pub struct Attest {
        pub magic: u32,
        pub attested_type: u16,
        pub extra_data: Vec<u8>,
        /// The certified object's name: its hash algorithm and digest.
        pub name: Vec<u8>,
    }

    impl Attest {
        pub fn parse(bytes: &[u8]) -> Result<Self> {
            let mut r = Reader(bytes);
            let magic = r.u32()?;
            let attested_type = r.u16()?;
            r.sized()?; // qualifiedSigner
            let extra_data = r.sized()?.to_vec();
            r.take(25)?; // clockInfo and firmwareVersion
            if attested_type != ST_ATTEST_CERTIFY {
                return invalid("tpm attestation is not a certification");
            }
            // The name's size field is read and then not used: the digest's
            // length follows from its algorithm.
            r.u16()?;
            let hash_alg = r.u16()?;
            let digest_len = match hash_alg {
                ALG_SHA1 => 20,
                ALG_SHA256 => 32,
                _ => return invalid("unsupported tpm name algorithm"),
            };
            let digest = r.take(digest_len)?;
            r.sized()?; // qualifiedName
            let mut name = hash_alg.to_be_bytes().to_vec();
            name.extend_from_slice(digest);
            Ok(Self {
                magic,
                attested_type,
                extra_data,
                name,
            })
        }
    }

    enum Parameters {
        Ecc {
            symmetric: u16,
            scheme: u16,
            curve: u16,
        },
        Rsa {
            symmetric: u16,
            scheme: u16,
            key_bits: u16,
        },
    }

    /// `TPMT_PUBLIC`.
    pub struct Public {
        name_alg: u16,
        parameters: Parameters,
        unique: (Vec<u8>, Vec<u8>),
    }

    impl Public {
        pub fn parse(bytes: &[u8]) -> Result<Self> {
            let mut r = Reader(bytes);
            let alg_type = r.u16()?;
            let name_alg = r.u16()?;
            r.take(4)?; // objectAttributes
            r.sized()?; // authPolicy
            let parameters = match alg_type {
                ALG_ECC => {
                    let symmetric = r.u16()?;
                    let scheme = r.u16()?;
                    let curve = r.u16()?;
                    r.u16()?; // kdf
                    Parameters::Ecc {
                        symmetric,
                        scheme,
                        curve,
                    }
                }
                ALG_RSA => {
                    let symmetric = r.u16()?;
                    let scheme = r.u16()?;
                    let key_bits = r.u16()?;
                    r.u32()?; // exponent, which the gem does not use
                    Parameters::Rsa {
                        symmetric,
                        scheme,
                        key_bits,
                    }
                }
                _ => return invalid("unsupported tpm key type"),
            };
            let unique = match parameters {
                Parameters::Ecc { .. } => (r.sized()?.to_vec(), r.sized()?.to_vec()),
                Parameters::Rsa { .. } => (r.sized()?.to_vec(), Vec::new()),
            };
            Ok(Self {
                name_alg,
                parameters,
                unique,
            })
        }

        /// The name the TPM gives this object: its name algorithm and the
        /// digest of the whole structure.
        pub fn name(&self, bytes: &[u8]) -> Result<Vec<u8>> {
            let hash = match self.name_alg {
                ALG_SHA1 => Hash::Sha1,
                ALG_SHA256 => Hash::Sha256,
                _ => return invalid("unsupported tpm name algorithm"),
            };
            let mut name = self.name_alg.to_be_bytes().to_vec();
            name.extend(hash.digest(bytes));
            Ok(name)
        }

        /// `TPublic#key`: the key, or `None` where the gem has none. The
        /// RSA exponent is always 65537, as the gem has it.
        pub fn key(&self) -> Result<Option<PublicKey>> {
            match self.parameters {
                Parameters::Ecc {
                    symmetric,
                    scheme,
                    curve,
                } => {
                    if symmetric != ALG_NULL || !matches!(scheme, ALG_ECDSA | ALG_NULL) {
                        return Ok(None);
                    }
                    let curve = match curve {
                        ECC_NIST_P256 => Some(Curve::P256),
                        ECC_NIST_P384 => Some(Curve::P384),
                        // A P-521 key can never be a credential eunha
                        // accepts, so it does not match one.
                        ECC_NIST_P521 => None,
                        _ => return Err(Error::Invalid("unknown tpm curve")),
                    };
                    let Some(curve) = curve else {
                        return Ok(Some(PublicKey::Unsupported));
                    };
                    let mut point = vec![0x04];
                    point.extend_from_slice(&self.unique.0);
                    point.extend_from_slice(&self.unique.1);
                    Ok(PublicKey::ec(curve, &point))
                }
                Parameters::Rsa {
                    symmetric,
                    scheme,
                    key_bits,
                } => {
                    if symmetric != ALG_NULL
                        || !matches!(scheme, ALG_RSASSA | ALG_RSAPSS | ALG_NULL)
                        || usize::from(key_bits / 8) != self.unique.0.len()
                    {
                        return Ok(None);
                    }
                    Ok(Some(PublicKey::rsa(&self.unique.0, &[0x01, 0x00, 0x01])))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{decode, encode, verify_registration_with, AuthenticatorData, RelyingParty};
    use super::*;
    use chrono::TimeZone as _;

    /// A registration webauthn-ruby's own specs verify, from a real
    /// authenticator.
    struct Vector {
        rp: RelyingParty,
        challenge: String,
        object: Vec<u8>,
        client_data: Vec<u8>,
    }

    fn vector(name: &str) -> Vector {
        let all: Json = serde_json::from_str(include_str!("testdata/vectors.json")).unwrap();
        let v = &all[name];
        let text = |key: &str| v[key].as_str().unwrap().to_owned();
        Vector {
            rp: RelyingParty {
                id: text("rp_id"),
                origin: text("origin"),
            },
            challenge: text("challenge"),
            object: decode(&text("attestation_object")).unwrap(),
            client_data: decode(&text("client_data_json")).unwrap(),
        }
    }

    impl Vector {
        fn register(&self, trust: &Trust) -> Result<super::super::NewCredential> {
            self.register_object(&self.object, trust)
        }

        fn register_object(
            &self,
            object: &[u8],
            trust: &Trust,
        ) -> Result<super::super::NewCredential> {
            let (map, _) = super::super::cbor::parse(object).unwrap();
            let auth_data = map.get_text("authData").and_then(Value::as_bytes).unwrap();
            let data = AuthenticatorData::parse(auth_data).unwrap();
            let id = encode(&data.credential.unwrap().id);
            let credential = serde_json::json!({
                "id": id,
                "rawId": id,
                "type": "public-key",
                "response": {
                    "clientDataJSON": encode(&self.client_data),
                    "attestationObject": encode(object),
                },
            });
            verify_registration_with(&credential, &self.challenge, &self.rp, trust)
        }

        /// The attestation object with its statement changed.
        fn with_statement(&self, change: impl FnOnce(&mut Vec<(Value, Value)>)) -> Vec<u8> {
            self.with_object(|object| {
                let statement = object
                    .iter_mut()
                    .find(|(k, _)| *k == Value::Text("attStmt".into()))
                    .unwrap();
                let Value::Map(entries) = &mut statement.1 else {
                    panic!("statement is not a map");
                };
                change(entries);
            })
        }

        fn with_object(&self, change: impl FnOnce(&mut Vec<(Value, Value)>)) -> Vec<u8> {
            let (Value::Map(mut object), _) = super::super::cbor::parse(&self.object).unwrap()
            else {
                panic!("attestation object is not a map");
            };
            change(&mut object);
            super::super::cbor::encode(&Value::Map(object))
        }
    }

    fn set(entries: &mut Vec<(Value, Value)>, key: &str, value: Value) {
        entries.retain(|(k, _)| *k != Value::Text(key.into()));
        entries.push((Value::Text(key.into()), value));
    }

    fn flip_last_byte(entries: &mut [(Value, Value)], key: &str) {
        let (_, Value::Bytes(bytes)) = entries
            .iter_mut()
            .find(|(k, _)| *k == Value::Text(key.into()))
            .unwrap()
        else {
            panic!("{key} is not bytes");
        };
        *bytes.last_mut().unwrap() ^= 1;
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
    }

    /// Mastodon's configuration at a moment.
    fn mastodon_at(now: DateTime<Utc>) -> Trust {
        Trust { now, roots: None }
    }

    /// Roots configured, as webauthn-ruby's specs configure a finder.
    fn roots_at(now: DateTime<Utc>, root: &[u8]) -> Trust {
        Trust {
            now,
            roots: Some(vec![root.to_vec()]),
        }
    }

    #[test]
    fn checks_packed_self_attestation() {
        let v = vector("security_key_packed_self");
        v.register(&Trust::mastodon()).unwrap();

        let tampered = v.with_statement(|s| flip_last_byte(s, "sig"));
        assert!(v.register_object(&tampered, &Trust::mastodon()).is_err());
        // The algorithm must be the new key's.
        let other_alg = v.with_statement(|s| set(s, "alg", Value::Int(-257)));
        assert!(v.register_object(&other_alg, &Trust::mastodon()).is_err());
        let no_alg = v.with_statement(|s| s.retain(|(k, _)| *k != Value::Text("alg".into())));
        assert!(v.register_object(&no_alg, &Trust::mastodon()).is_err());
    }

    #[test]
    fn refuses_packed_basic_attestation_without_roots() {
        let v = vector("security_key_packed_x5c");
        let when = at(2024, 1, 1, 0, 0, 0);
        // Mastodon configures no roots, so the chain cannot be trusted.
        assert!(v.register(&mastodon_at(when)).is_err());

        // With Yubico's root, as webauthn-ruby's spec configures it, the
        // statement holds, and a broken signature does not.
        let yubico = include_bytes!("testdata/yubico_u2f_root.der");
        v.register(&roots_at(when, yubico)).unwrap();
        let tampered = v.with_statement(|s| flip_last_byte(s, "sig"));
        assert!(v
            .register_object(&tampered, &roots_at(when, yubico))
            .is_err());
        // Another root does not make it trusted.
        let feitian = include_bytes!("testdata/feitian_ft_fido_0200.der");
        assert!(v.register(&roots_at(when, feitian)).is_err());
        // A U2F certificate without the packed requirements is refused.
        let u2f = vector("security_key_direct");
        let (u2f_object, _) = super::super::cbor::parse(&u2f.object).unwrap();
        let u2f_x5c = u2f_object
            .get_text("attStmt")
            .unwrap()
            .get_text("x5c")
            .unwrap()
            .clone();
        let swapped = v.with_statement(|s| set(s, "x5c", u2f_x5c));
        assert!(v
            .register_object(&swapped, &roots_at(when, feitian))
            .is_err());
    }

    #[test]
    fn refuses_fido_u2f_attestation_without_roots() {
        let v = vector("security_key_direct");
        let when = at(2024, 1, 1, 0, 0, 0);
        assert!(v.register(&mastodon_at(when)).is_err());

        let feitian = include_bytes!("testdata/feitian_ft_fido_0200.der");
        v.register(&roots_at(when, feitian)).unwrap();
        let tampered = v.with_statement(|s| flip_last_byte(s, "sig"));
        assert!(v
            .register_object(&tampered, &roots_at(when, feitian))
            .is_err());
        let two = v.with_statement(|s| {
            let Some((_, Value::Array(x5c))) =
                s.iter_mut().find(|(k, _)| *k == Value::Text("x5c".into()))
            else {
                panic!("no x5c");
            };
            x5c.push(x5c[0].clone());
        });
        assert!(v.register_object(&two, &roots_at(when, feitian)).is_err());
    }

    #[test]
    fn checks_android_key_attestation() {
        let v = vector("android_key_direct");
        let when = at(2024, 1, 1, 0, 0, 0);
        let root = include_bytes!("testdata/android_key_root.der");
        v.register(&roots_at(when, root)).unwrap();

        // The vector comes from an emulator, whose chain does not reach the
        // Google root the gem ships; and that root expired in May 2026.
        assert!(v.register(&mastodon_at(when)).is_err());
        assert!(v.register(&Trust::mastodon()).is_err());
        let tampered = v.with_statement(|s| flip_last_byte(s, "sig"));
        assert!(v.register_object(&tampered, &roots_at(when, root)).is_err());
        let rsa = v.with_statement(|s| set(s, "alg", Value::Int(-257)));
        assert!(v.register_object(&rsa, &roots_at(when, root)).is_err());
        assert!(v
            .register(&roots_at(at(2100, 1, 1, 0, 0, 0), root))
            .is_err());
    }

    #[test]
    fn google_root_for_android_key_has_expired() {
        let root = Certificate::from_der(ANDROID_KEY_ROOTS[0]).unwrap();
        assert!(root.in_validity_period(at(2026, 5, 1, 0, 0, 0)));
        assert!(!root.in_validity_period(at(2026, 5, 25, 0, 0, 0)));
    }

    #[test]
    fn checks_android_safetynet_attestation() {
        let v = vector("android_safetynet_direct");
        let when = at(2019, 7, 7, 16, 15, 11);
        v.register(&mastodon_at(when)).unwrap();

        // The response is good for a minute either side.
        assert!(v
            .register(&mastodon_at(at(2019, 7, 7, 16, 17, 11)))
            .is_err());
        assert!(v.register(&Trust::mastodon()).is_err());
        let no_version = v.with_statement(|s| set(s, "ver", Value::Text(String::new())));
        assert!(v.register_object(&no_version, &mastodon_at(when)).is_err());
        let tampered = v.with_statement(|s| flip_last_byte(s, "response"));
        assert!(v.register_object(&tampered, &mastodon_at(when)).is_err());
        // Only Google's roots are trusted.
        let yubico = include_bytes!("testdata/yubico_u2f_root.der");
        assert!(v.register(&roots_at(when, yubico)).is_err());
    }

    #[test]
    fn checks_tpm_attestation() {
        let v = vector("tpm");
        let when = at(2025, 6, 1, 0, 0, 0);
        v.register(&mastodon_at(when)).unwrap();

        // The AIK certificate must be in use.
        assert!(v.register(&mastodon_at(at(2031, 1, 1, 0, 0, 0))).is_err());
        let version = v.with_statement(|s| set(s, "ver", Value::Text("1.0".into())));
        assert!(v.register_object(&version, &mastodon_at(when)).is_err());
        let cert_info = v.with_statement(|s| flip_last_byte(s, "certInfo"));
        assert!(v.register_object(&cert_info, &mastodon_at(when)).is_err());
        let pub_area = v.with_statement(|s| flip_last_byte(s, "pubArea"));
        assert!(v.register_object(&pub_area, &mastodon_at(when)).is_err());
        let signature = v.with_statement(|s| flip_last_byte(s, "sig"));
        assert!(v.register_object(&signature, &mastodon_at(when)).is_err());
        let algorithm = v.with_statement(|s| set(s, "alg", Value::Int(-257)));
        assert!(v.register_object(&algorithm, &mastodon_at(when)).is_err());
        // Only the TPM vendors' roots are trusted.
        let yubico = include_bytes!("testdata/yubico_u2f_root.der");
        assert!(v.register(&roots_at(when, yubico)).is_err());
    }

    #[test]
    fn checks_apple_attestation() {
        let v = vector("macbook_touch_id");
        // The credential certificate lasts three days.
        let when = at(2021, 2, 23, 0, 0, 0);
        v.register(&mastodon_at(when)).unwrap();
        assert!(v.register(&Trust::mastodon()).is_err());

        let packed = vector("security_key_packed_x5c");
        let (packed_object, _) = super::super::cbor::parse(&packed.object).unwrap();
        let other_x5c = packed_object
            .get_text("attStmt")
            .unwrap()
            .get_text("x5c")
            .unwrap()
            .clone();
        let swapped = v.with_statement(|s| set(s, "x5c", other_x5c));
        assert!(v.register_object(&swapped, &mastodon_at(when)).is_err());
    }

    #[test]
    fn refuses_unknown_formats_and_none_with_a_statement() {
        let v = vector("security_key_packed_self");
        let unknown = v.with_object(|o| set(o, "fmt", Value::Text("made-up".into())));
        assert!(v.register_object(&unknown, &Trust::mastodon()).is_err());
        // `none` with a statement is refused; the self attestation's own
        // statement is not empty.
        let none = v.with_object(|o| set(o, "fmt", Value::Text("none".into())));
        assert!(v.register_object(&none, &Trust::mastodon()).is_err());
        let empty = v.with_object(|o| {
            set(o, "fmt", Value::Text("none".into()));
            set(o, "attStmt", Value::Map(vec![]));
        });
        v.register_object(&empty, &Trust::mastodon()).unwrap();
    }

    #[test]
    fn every_shipped_root_parses() {
        for der in [TPM_ROOTS, SAFETYNET_ROOTS, ANDROID_KEY_ROOTS, APPLE_ROOTS].concat() {
            // One of the TPM "roots", Infineon's EK root, is issued by
            // VeriSign, which the gem does not ship; OpenSSL trusts no
            // chain that ends there, and neither does eunha.
            let root = Certificate::from_der(der).unwrap();
            assert!(!matches!(root.public_key, PublicKey::Unsupported));
        }
    }
}
