//! X.509 certificates, and checking a chain of them the way an
//! `OpenSSL::X509::Store` with a few roots added does when webauthn-ruby
//! and its attestation gems call `verify`.
//!
//! The chain is built from the leaf up, trust store first, then the
//! certificates the authenticator sent. Every certificate in it, the root
//! included, must be within its validity period; every one above the leaf
//! must be a CA, with `basicConstraints` saying so for all but the root, and
//! its path length and key usage allowing it; a critical extension OpenSSL
//! does not know refuses the chain; and each signature must hold. The root's
//! own signature is not checked, as OpenSSL does not check it.

use chrono::{DateTime, NaiveDate, Utc};

use super::crypto::{Curve, EcdsaEncoding, Hash, PublicKey};
use super::der::{self, Tlv};
use super::{invalid, Error, Result};

pub mod oids {
    pub const RSA_ENCRYPTION: &str = "1.2.840.113549.1.1.1";
    pub const EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
    pub const P256: &str = "1.2.840.10045.3.1.7";
    pub const P384: &str = "1.3.132.0.34";

    pub const SHA1_RSA: &str = "1.2.840.113549.1.1.5";
    pub const SHA256_RSA: &str = "1.2.840.113549.1.1.11";
    pub const SHA384_RSA: &str = "1.2.840.113549.1.1.12";
    pub const SHA512_RSA: &str = "1.2.840.113549.1.1.13";
    pub const ECDSA_SHA1: &str = "1.2.840.10045.4.1";
    pub const ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
    pub const ECDSA_SHA384: &str = "1.2.840.10045.4.3.3";
    pub const ECDSA_SHA512: &str = "1.2.840.10045.4.3.4";

    pub const COMMON_NAME: &str = "2.5.4.3";
    pub const ORGANIZATIONAL_UNIT: &str = "2.5.4.11";

    pub const SUBJECT_KEY_IDENTIFIER: &str = "2.5.29.14";
    pub const KEY_USAGE: &str = "2.5.29.15";
    pub const SUBJECT_ALT_NAME: &str = "2.5.29.17";
    pub const BASIC_CONSTRAINTS: &str = "2.5.29.19";
    pub const NAME_CONSTRAINTS: &str = "2.5.29.30";
    pub const CERTIFICATE_POLICIES: &str = "2.5.29.32";
    pub const POLICY_MAPPINGS: &str = "2.5.29.33";
    pub const AUTHORITY_KEY_IDENTIFIER: &str = "2.5.29.35";
    pub const POLICY_CONSTRAINTS: &str = "2.5.29.36";
    pub const EXT_KEY_USAGE: &str = "2.5.29.37";
    pub const INHIBIT_ANY_POLICY: &str = "2.5.29.54";
    pub const NETSCAPE_CERT_TYPE: &str = "2.16.840.1.113730.1.1";
    pub const IP_ADDR_BLOCKS: &str = "1.3.6.1.5.5.7.1.7";
    pub const AUTONOMOUS_SYS_IDS: &str = "1.3.6.1.5.5.7.1.8";
    pub const PROXY_CERT_INFO: &str = "1.3.6.1.5.5.7.1.14";
}

/// The extensions OpenSSL handles, which may therefore be critical
/// (`X509_supported_extension`).
const SUPPORTED_CRITICAL: &[&str] = &[
    oids::NETSCAPE_CERT_TYPE,
    oids::KEY_USAGE,
    oids::SUBJECT_ALT_NAME,
    oids::BASIC_CONSTRAINTS,
    oids::CERTIFICATE_POLICIES,
    oids::EXT_KEY_USAGE,
    oids::IP_ADDR_BLOCKS,
    oids::AUTONOMOUS_SYS_IDS,
    oids::POLICY_CONSTRAINTS,
    oids::PROXY_CERT_INFO,
    oids::NAME_CONSTRAINTS,
    oids::POLICY_MAPPINGS,
    oids::INHIBIT_ANY_POLICY,
];

/// The deepest chain OpenSSL builds by default.
const MAX_DEPTH: usize = 100;

const KU_KEY_CERT_SIGN: u16 = 0x0004;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub oid: Vec<u8>,
    pub tag: u32,
    pub value: Vec<u8>,
}

/// A distinguished name.
#[derive(Debug, Clone)]
pub struct Name {
    pub rdns: Vec<Vec<Attribute>>,
}

impl Name {
    pub fn parse(tlv: &Tlv<'_>) -> Result<Self> {
        let mut rdns = Vec::new();
        for rdn in tlv.sequence()? {
            if !rdn.is_universal(der::SET) {
                return invalid("name entry is not a SET");
            }
            let mut attributes = Vec::new();
            for atv in rdn.children()? {
                let parts = atv.sequence()?;
                let [oid, value] = parts.as_slice() else {
                    return invalid("malformed name attribute");
                };
                attributes.push(Attribute {
                    oid: oid.oid()?.to_vec(),
                    tag: value.tag,
                    value: value.value.to_vec(),
                });
            }
            rdns.push(attributes);
        }
        Ok(Self { rdns })
    }

    pub fn is_empty(&self) -> bool {
        self.rdns.is_empty()
    }

    /// The attributes in order, as `OpenSSL::X509::Name#to_a` lists them.
    pub fn attributes(&self) -> impl Iterator<Item = &Attribute> {
        self.rdns.iter().flatten()
    }

    /// The raw value of the first attribute of this type, as
    /// `name.to_a.assoc(type)&.at(1)` finds it.
    pub fn first(&self, oid: &str) -> Option<&[u8]> {
        let oid = der::oid(oid);
        self.attributes()
            .find(|a| a.oid == oid)
            .map(|a| a.value.as_slice())
    }

    /// The form `X509_NAME_cmp` compares: string values in UTF-8, trimmed,
    /// inner whitespace collapsed and lower-cased.
    fn canonical(&self) -> Vec<Vec<CanonicalAttribute>> {
        self.rdns
            .iter()
            .map(|rdn| {
                rdn.iter()
                    .map(|a| CanonicalAttribute {
                        oid: a.oid.clone(),
                        value: canonical_string(a.tag, &a.value)
                            .ok_or_else(|| (a.tag, a.value.clone())),
                    })
                    .collect()
            })
            .collect()
    }

    pub fn same_as(&self, other: &Self) -> bool {
        self.canonical() == other.canonical()
    }
}

/// An attribute as names are compared: a string value in canonical form,
/// anything else as it was encoded.
#[derive(PartialEq)]
struct CanonicalAttribute {
    oid: Vec<u8>,
    value: std::result::Result<String, (u32, Vec<u8>)>,
}

fn canonical_string(tag: u32, value: &[u8]) -> Option<String> {
    let text: String = match tag {
        // UTF8String, PrintableString, T61String, IA5String, VisibleString
        12 | 19 | 26 | 22 => String::from_utf8_lossy(value).into_owned(),
        20 => value.iter().map(|b| char::from(*b)).collect(),
        // BMPString
        30 => {
            let units: Vec<u16> = value
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        // UniversalString
        28 => value
            .chunks(4)
            .filter_map(|c| {
                let mut b = [0u8; 4];
                b[..c.len()].copy_from_slice(c);
                char::from_u32(u32::from_be_bytes(b))
            })
            .collect(),
        _ => return None,
    };
    let collapsed = text.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
    Some(collapsed.to_ascii_lowercase())
}

#[derive(Debug, Clone)]
pub struct Extension {
    pub oid: Vec<u8>,
    pub critical: bool,
    /// The contents of `extnValue`.
    pub value: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Certificate {
    pub der: Vec<u8>,
    tbs: Vec<u8>,
    tbs_signature_algorithm: Vec<u8>,
    signature_algorithm: Vec<u8>,
    signature_oid: Vec<u8>,
    signature: Vec<u8>,
    /// As X.509 numbers it: 2 is v3.
    pub version: u64,
    pub serial: Vec<u8>,
    pub issuer: Name,
    pub subject: Name,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub public_key: PublicKey,
    pub extensions: Vec<Extension>,
}

impl Certificate {
    pub fn from_der(bytes: &[u8]) -> Result<Self> {
        let cert = Tlv::parse(bytes)?;
        let parts = cert.sequence()?;
        let [tbs, signature_algorithm, signature] = parts.as_slice() else {
            return invalid("malformed certificate");
        };
        let signature_oid = signature_algorithm
            .sequence()?
            .first()
            .ok_or(Error::Invalid("malformed signature algorithm"))?
            .oid()?
            .to_vec();
        let signature = signature.bits()?.to_vec();

        let mut fields = tbs.sequence()?.into_iter().peekable();
        let mut version = 0;
        if let Some(v) = fields.peek().filter(|f| f.is(der::CLASS_CONTEXT, 0)) {
            version = Tlv::parse(v.value)?.small_uint()?;
            fields.next();
        }
        let mut next = || {
            fields
                .next()
                .ok_or(Error::Invalid("certificate ends early"))
        };
        let serial = next()?;
        if !serial.is_universal(der::INTEGER) {
            return invalid("malformed serial number");
        }
        let tbs_signature_algorithm = next()?;
        let issuer = Name::parse(&next()?)?;
        let validity = next()?.sequence()?;
        let [not_before, not_after] = validity.as_slice() else {
            return invalid("malformed validity");
        };
        let subject = Name::parse(&next()?)?;
        let public_key = parse_spki(&next()?)?;
        let mut extensions = Vec::new();
        for field in fields {
            if field.is(der::CLASS_CONTEXT, 3) {
                for ext in Tlv::parse(field.value)?.sequence()? {
                    let parts = ext.sequence()?;
                    let (oid, critical, value) = match parts.as_slice() {
                        [oid, value] => (oid, false, value),
                        [oid, critical, value] => (oid, critical.boolean()?, value),
                        _ => return invalid("malformed extension"),
                    };
                    extensions.push(Extension {
                        oid: oid.oid()?.to_vec(),
                        critical,
                        value: value.octets()?.to_vec(),
                    });
                }
            }
        }

        Ok(Self {
            der: bytes.to_vec(),
            tbs: tbs.raw.to_vec(),
            tbs_signature_algorithm: tbs_signature_algorithm.raw.to_vec(),
            signature_algorithm: signature_algorithm.raw.to_vec(),
            signature_oid,
            signature,
            version,
            serial: serial.value.to_vec(),
            issuer,
            subject,
            not_before: parse_time(not_before)?,
            not_after: parse_time(not_after)?,
            public_key,
            extensions,
        })
    }

    /// The first extension of this type, as `find_extension` finds it.
    pub fn extension(&self, oid: &str) -> Option<&Extension> {
        let oid = der::oid(oid);
        self.extensions.iter().find(|e| e.oid == oid)
    }

    /// `basicConstraints` as (cA, pathLenConstraint).
    pub fn basic_constraints(&self) -> Result<Option<(bool, Option<u64>)>> {
        let Some(ext) = self.extension(oids::BASIC_CONSTRAINTS) else {
            return Ok(None);
        };
        let mut ca = false;
        let mut path_len = None;
        for part in Tlv::parse(&ext.value)?.sequence()? {
            if part.is_universal(der::BOOLEAN) {
                ca = part.boolean()?;
            } else {
                path_len = Some(part.small_uint()?);
            }
        }
        Ok(Some((ca, path_len)))
    }

    /// Whether `basicConstraints` reads `CA:FALSE` in OpenSSL's words: there,
    /// not a CA, and with no path length.
    pub fn basic_constraints_ca_false(&self) -> bool {
        matches!(self.basic_constraints(), Ok(Some((false, None))))
    }

    fn key_usage(&self) -> Result<Option<u16>> {
        let Some(ext) = self.extension(oids::KEY_USAGE) else {
            return Ok(None);
        };
        let tlv = Tlv::parse(&ext.value)?;
        if !tlv.is_universal(der::BIT_STRING) || tlv.value.is_empty() {
            return invalid("malformed key usage");
        }
        // OpenSSL keeps the first two bytes as they come: keyCertSign, bit
        // 5, is 0x04 of the first.
        let bits = &tlv.value[1..];
        let first = u16::from(bits.first().copied().unwrap_or(0));
        let second = u16::from(bits.get(1).copied().unwrap_or(0));
        Ok(Some(first | (second << 8)))
    }

    fn subject_key_id(&self) -> Option<Vec<u8>> {
        let ext = self.extension(oids::SUBJECT_KEY_IDENTIFIER)?;
        Tlv::parse(&ext.value)
            .ok()?
            .octets()
            .ok()
            .map(<[u8]>::to_vec)
    }

    /// The `authorityKeyIdentifier`'s key id and serial number.
    fn authority_key_id(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let Some(ext) = self.extension(oids::AUTHORITY_KEY_IDENTIFIER) else {
            return (None, None);
        };
        let Ok(parts) = Tlv::parse(&ext.value).and_then(|t| t.sequence()) else {
            return (None, None);
        };
        let mut key_id = None;
        let mut serial = None;
        for part in parts {
            if part.is(der::CLASS_CONTEXT, 0) {
                key_id = Some(part.value.to_vec());
            } else if part.is(der::CLASS_CONTEXT, 2) {
                serial = Some(part.value.to_vec());
            }
        }
        (key_id, serial)
    }

    /// Whether the extensions decode as OpenSSL needs them to; a certificate
    /// whose do not is refused as `X509_V_ERR_INVALID_EXTENSION`.
    fn extensions_valid(&self) -> bool {
        let mut seen: Vec<&[u8]> = Vec::new();
        for ext in &self.extensions {
            if seen.contains(&ext.oid.as_slice()) {
                return false;
            }
            seen.push(&ext.oid);
        }
        let constraints = self.basic_constraints();
        if constraints.is_err() || matches!(constraints, Ok(Some((false, Some(_))))) {
            return false;
        }
        self.key_usage().is_ok()
    }

    /// `X509_check_akid`: the authority key identifier, if any, names this
    /// issuer.
    fn akid_matches(&self, issuer: &Self) -> bool {
        let (key_id, serial) = self.authority_key_id();
        if let (Some(key_id), Some(skid)) = (key_id, issuer.subject_key_id()) {
            if key_id != skid {
                return false;
            }
        }
        if let Some(serial) = serial {
            if serial != issuer.serial {
                return false;
            }
        }
        true
    }

    /// Issuer and subject the same, and the key identifiers agreeing.
    pub fn self_issued(&self) -> bool {
        self.subject.same_as(&self.issuer) && self.akid_matches(self)
    }

    /// `x509_likely_issued` and `X509_check_issued`: whether `issuer` could
    /// have issued this certificate, before its signature is checked.
    fn could_be_issued_by(&self, issuer: &Self) -> bool {
        if !issuer.subject.same_as(&self.issuer) || !self.akid_matches(issuer) {
            return false;
        }
        !matches!(issuer.key_usage(), Ok(Some(ku)) if ku & KU_KEY_CERT_SIGN == 0)
    }

    pub fn in_validity_period(&self, now: DateTime<Utc>) -> bool {
        self.not_before <= now && now < self.not_after
    }

    /// Whether `issuer`'s key made this certificate's signature.
    pub fn signed_by(&self, issuer: &PublicKey) -> bool {
        if self.signature_algorithm != self.tbs_signature_algorithm {
            return false;
        }
        let oid = &self.signature_oid;
        let is = |dotted: &str| *oid == der::oid(dotted);
        let rsa = |hash| issuer.verify_pkcs1(hash, &self.tbs, &self.signature);
        let ecdsa =
            |hash| issuer.verify_ecdsa(hash, &self.tbs, &self.signature, EcdsaEncoding::Der);
        if is(oids::SHA1_RSA) {
            rsa(Hash::Sha1)
        } else if is(oids::SHA256_RSA) {
            rsa(Hash::Sha256)
        } else if is(oids::SHA384_RSA) {
            rsa(Hash::Sha384)
        } else if is(oids::SHA512_RSA) {
            rsa(Hash::Sha512)
        } else if is(oids::ECDSA_SHA1) {
            ecdsa(Hash::Sha1)
        } else if is(oids::ECDSA_SHA256) {
            ecdsa(Hash::Sha256)
        } else if is(oids::ECDSA_SHA384) {
            ecdsa(Hash::Sha384)
        } else if is(oids::ECDSA_SHA512) {
            ecdsa(Hash::Sha512)
        } else {
            false
        }
    }

    /// `X509_check_ca`: 0 when not a CA, 1 when `basicConstraints` says it
    /// is one, 3 for a self-signed v1 root, 4 when only the key usage allows
    /// signing certificates.
    fn check_ca(&self) -> u8 {
        let key_usage = self.key_usage().ok().flatten();
        if key_usage.is_some_and(|ku| ku & KU_KEY_CERT_SIGN == 0) {
            return 0;
        }
        match self.basic_constraints() {
            Ok(Some((ca, _))) => u8::from(ca),
            Ok(None) if self.version == 0 && self.self_issued() => 3,
            Ok(None) if key_usage.is_some() => 4,
            _ => 0,
        }
    }

    fn unhandled_critical_extension(&self) -> bool {
        self.extensions
            .iter()
            .any(|e| e.critical && !SUPPORTED_CRITICAL.iter().any(|oid| der::oid(oid) == e.oid))
    }
}

fn parse_spki(spki: &Tlv<'_>) -> Result<PublicKey> {
    let parts = spki.sequence()?;
    let [algorithm, key] = parts.as_slice() else {
        return invalid("malformed public key");
    };
    let algorithm = algorithm.sequence()?;
    let oid = algorithm
        .first()
        .ok_or(Error::Invalid("malformed public key algorithm"))?
        .oid()?;
    let bits = key.bits()?;
    if oid == der::oid(oids::RSA_ENCRYPTION) {
        let parts = Tlv::parse(bits)?.sequence()?;
        let [n, e] = parts.as_slice() else {
            return invalid("malformed RSA key");
        };
        return Ok(PublicKey::rsa(n.unsigned_bytes()?, e.unsigned_bytes()?));
    }
    if oid == der::oid(oids::EC_PUBLIC_KEY) {
        let curve = algorithm.get(1).and_then(|p| p.oid().ok());
        let curve = match curve {
            Some(c) if c == der::oid(oids::P256) => Curve::P256,
            Some(c) if c == der::oid(oids::P384) => Curve::P384,
            _ => return Ok(PublicKey::Unsupported),
        };
        return Ok(PublicKey::ec(curve, bits).unwrap_or(PublicKey::Unsupported));
    }
    Ok(PublicKey::Unsupported)
}

fn parse_time(tlv: &Tlv<'_>) -> Result<DateTime<Utc>> {
    let text = std::str::from_utf8(tlv.value).or_else(|_| invalid("malformed time"))?;
    let (year, rest) = if tlv.is_universal(der::UTC_TIME) && text.len() == 13 {
        let yy: i32 = text[..2].parse().or_else(|_| invalid("malformed time"))?;
        (if yy < 50 { 2000 + yy } else { 1900 + yy }, &text[2..])
    } else if tlv.is_universal(der::GENERALIZED_TIME) && text.len() == 15 {
        (
            text[..4].parse().or_else(|_| invalid("malformed time"))?,
            &text[4..],
        )
    } else {
        return invalid("malformed time");
    };
    if !rest.ends_with('Z') || !rest[..10].bytes().all(|b| b.is_ascii_digit()) {
        return invalid("malformed time");
    }
    let num = |i: usize| rest[i..i + 2].parse::<u32>().unwrap_or(99);
    NaiveDate::from_ymd_opt(year, num(0), num(2))
        .and_then(|d| d.and_hms_opt(num(4), num(6), num(8)))
        .map(|t| t.and_utc())
        .ok_or(Error::Invalid("malformed time"))
}

/// Parse each DER certificate, refusing the lot if one does not parse.
pub fn parse_all(ders: &[Vec<u8>]) -> Result<Vec<Certificate>> {
    ders.iter().map(|d| Certificate::from_der(d)).collect()
}

/// `store.verify(leaf, untrusted)` with `trusted` added to the store and its
/// time set to `now`: the chain from the leaf to a trusted root, or an error.
pub fn verify_chain(
    leaf: &Certificate,
    untrusted: &[Certificate],
    trusted: &[Certificate],
    now: DateTime<Utc>,
) -> Result<Vec<Certificate>> {
    let mut chain = vec![leaf.clone()];
    loop {
        if chain.len() > MAX_DEPTH {
            return invalid("certificate chain is too long");
        }
        let current = chain.last().expect("never empty");
        if current.self_issued() {
            if trusted.iter().any(|t| t.der == current.der) {
                break;
            }
            // An untrusted copy of a root gives way to the trusted one.
            if let Some(root) = find_issuer(current, trusted, now) {
                if root.self_issued() {
                    let root = root.clone();
                    chain.pop();
                    chain.push(root);
                    break;
                }
            }
            return invalid("certificate chain ends in an untrusted self-signed certificate");
        }
        let issuer = find_issuer(current, trusted, now).or_else(|| {
            let remaining: Vec<&Certificate> = untrusted
                .iter()
                .filter(|c| !chain.iter().any(|in_chain| in_chain.der == c.der))
                .collect();
            find_issuer_in(current, remaining, now)
        });
        match issuer {
            Some(issuer) => chain.push(issuer.clone()),
            None => return invalid("unable to get the issuer certificate"),
        }
    }

    let last = chain.len() - 1;
    let mut path_len = 0u64;
    for (i, cert) in chain.iter().enumerate() {
        if !cert.in_validity_period(now) {
            return invalid("certificate is not within its validity period");
        }
        if !cert.extensions_valid() {
            return invalid("certificate has an invalid extension");
        }
        if cert.unhandled_critical_extension() {
            return invalid("certificate has an unhandled critical extension");
        }
        if i > 0 {
            let ca = cert.check_ca();
            if ca == 0 || (i < last && ca != 1) {
                return invalid("issuer is not a CA");
            }
        }
        if i > 1 {
            if let Ok(Some((_, Some(max)))) = cert.basic_constraints() {
                if path_len > max {
                    return invalid("path length constraint exceeded");
                }
            }
        }
        if i > 0 && !cert.subject.same_as(&cert.issuer) {
            path_len += 1;
        }
        if i < last && !cert.signed_by(&chain[i + 1].public_key) {
            return invalid("certificate signature does not verify");
        }
    }
    Ok(chain)
}

fn find_issuer<'a>(
    cert: &Certificate,
    candidates: &'a [Certificate],
    now: DateTime<Utc>,
) -> Option<&'a Certificate> {
    find_issuer_in(cert, candidates.iter().collect(), now)
}

/// The first candidate that could have issued `cert`, preferring one that
/// is within its validity period, as OpenSSL's `find_issuer` does.
fn find_issuer_in<'a>(
    cert: &Certificate,
    candidates: Vec<&'a Certificate>,
    now: DateTime<Utc>,
) -> Option<&'a Certificate> {
    let matching: Vec<&Certificate> = candidates
        .into_iter()
        .filter(|c| c.der != cert.der && cert.could_be_issued_by(c))
        .collect();
    matching
        .iter()
        .find(|c| c.in_validity_period(now))
        .or_else(|| matching.first())
        .copied()
}
