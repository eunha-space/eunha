//! Security keys, as Mastodon registers and checks them with webauthn-ruby.
//!
//! Mastodon stores what webauthn-ruby hands it, and eunha stores the same:
//! `webauthn_credentials.external_id` is the credential id as the browser
//! reported it (base64url), `public_key` the credential's COSE key, base64url
//! without padding, and `sign_count` the authenticator's counter.
//! `users.webauthn_id` is the opaque user handle, 64 random bytes base64url.
//!
//! The relying party is the instance's domain, the origin `https://` and the
//! domain, as `config/initializers/webauthn.rb` sets them. The algorithms
//! offered are webauthn-ruby's defaults: ES256, PS256 and RS256. User
//! verification is `discouraged`, as Mastodon asks for it.
//!
//! Attestation is not asked for, so browsers send the `none` format; a
//! `packed` self-attestation is checked against the new key. Other formats
//! are accepted without their statement being checked: they say which model
//! of authenticator made the key, which Mastodon does not act on either.

use base64::Engine as _;
use rand::RngCore as _;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

/// `config.rp_name`.
const RP_NAME: &str = "Mastodon";
/// `config.credential_options_timeout`.
const TIMEOUT_MS: u64 = 120_000;
/// COSE algorithm identifiers webauthn-ruby offers by default.
const ES256: i64 = -7;
const PS256: i64 = -37;
const RS256: i64 = -257;

const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_ATTESTED_CREDENTIAL: u8 = 0x40;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
}

type Result<T> = std::result::Result<T, Error>;

fn invalid<T>(why: &'static str) -> Result<T> {
    Err(Error::Invalid(why))
}

/// base64url without padding, the encoding webauthn-ruby uses throughout.
pub fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .or_else(|_| invalid("not base64url"))
}

/// `WebAuthn.generate_user_id`.
pub fn generate_user_id() -> String {
    random_b64(64)
}

/// A fresh challenge, as webauthn-ruby makes them: 32 random bytes.
pub fn generate_challenge() -> String {
    random_b64(32)
}

fn random_b64(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    encode(&bytes)
}

/// The relying party: its id and the one origin allowed.
#[derive(Debug, Clone)]
pub struct RelyingParty {
    pub id: String,
    pub origin: String,
}

impl RelyingParty {
    /// The instance's web domain over HTTPS.
    pub fn for_domain(domain: &str) -> Self {
        Self {
            id: domain.to_owned(),
            origin: format!("https://{domain}"),
        }
    }
}

/// `WebAuthn::Credential.options_for_create`, as JSON.
pub fn creation_options(
    challenge: &str,
    username: &str,
    user_handle: &str,
    exclude: &[String],
) -> Value {
    let params: Vec<Value> = [ES256, PS256, RS256]
        .iter()
        .map(|alg| json!({ "type": "public-key", "alg": alg }))
        .collect();
    json!({
        "challenge": challenge,
        "timeout": TIMEOUT_MS,
        "rp": { "name": RP_NAME },
        "user": { "name": username, "displayName": username, "id": user_handle },
        "pubKeyCredParams": params,
        "excludeCredentials": exclude
            .iter()
            .map(|id| json!({ "type": "public-key", "id": id }))
            .collect::<Vec<_>>(),
        "authenticatorSelection": { "userVerification": "discouraged" },
        "extensions": {},
    })
}

/// `WebAuthn::Credential.options_for_get`, as JSON.
pub fn request_options(challenge: &str, allow: &[String]) -> Value {
    json!({
        "challenge": challenge,
        "timeout": TIMEOUT_MS,
        "allowCredentials": allow
            .iter()
            .map(|id| json!({ "type": "public-key", "id": id }))
            .collect::<Vec<_>>(),
        "userVerification": "discouraged",
        "extensions": {},
    })
}

/// A key a browser has just made, checked and ready to store.
#[derive(Debug, Clone)]
pub struct NewCredential {
    pub external_id: String,
    pub public_key: String,
    pub sign_count: i64,
}

/// `WebAuthn::Credential.from_create(params).verify(challenge)`.
pub fn verify_registration(
    credential: &Value,
    challenge: &str,
    rp: &RelyingParty,
) -> Result<NewCredential> {
    let id = credential_id(credential)?;
    let response = credential
        .get("response")
        .ok_or(Error::Invalid("no response"))?;
    let client_data_json = decode(field(response, "clientDataJSON")?)?;
    verify_client_data(&client_data_json, "webauthn.create", challenge, rp)?;

    let attestation = decode(field(response, "attestationObject")?)?;
    let (object, _) = cbor::parse(&attestation)?;
    let fmt = object
        .get_text("fmt")
        .and_then(cbor::Value::as_text)
        .ok_or(Error::Invalid("attestation has no format"))?;
    let statement = object
        .get_text("attStmt")
        .ok_or(Error::Invalid("attestation has no statement"))?;
    let auth_data = object
        .get_text("authData")
        .and_then(cbor::Value::as_bytes)
        .ok_or(Error::Invalid("attestation has no authenticator data"))?;

    let data = AuthenticatorData::parse(auth_data)?;
    data.verify(rp)?;
    let attested = data
        .credential
        .as_ref()
        .ok_or(Error::Invalid("no attested credential"))?;
    if encode(&attested.id) != id.trim_end_matches('=') {
        return invalid("credential id does not match");
    }
    let key = CoseKey::parse(&attested.public_key)?;

    let client_data_hash = Sha256::digest(&client_data_json);
    match fmt {
        "none" => {}
        "packed" if statement.get_text("x5c").is_none() => {
            // Self attestation: signed with the new key itself.
            let alg = statement
                .get_text("alg")
                .and_then(cbor::Value::as_int)
                .ok_or(Error::Invalid("packed attestation has no algorithm"))?;
            if alg != key.alg {
                return invalid("attestation algorithm does not match the key");
            }
            let signature = statement
                .get_text("sig")
                .and_then(cbor::Value::as_bytes)
                .ok_or(Error::Invalid("packed attestation has no signature"))?;
            let mut signed = auth_data.to_vec();
            signed.extend_from_slice(&client_data_hash);
            key.verify(&signed, signature)?;
        }
        _ => {}
    }

    Ok(NewCredential {
        external_id: id.to_owned(),
        public_key: encode(&attested.public_key),
        sign_count: i64::from(data.sign_count),
    })
}

/// The id of the credential a sign-in used, to find which key to check it with.
pub fn credential_id(credential: &Value) -> Result<&str> {
    credential
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(Error::Invalid("credential has no id"))
}

/// `WebAuthn::Credential.from_get(params).verify(challenge, public_key:,
/// sign_count:)`: the new signature count when the assertion holds.
pub fn verify_assertion(
    credential: &Value,
    challenge: &str,
    rp: &RelyingParty,
    public_key: &str,
    stored_sign_count: i64,
) -> Result<i64> {
    let response = credential
        .get("response")
        .ok_or(Error::Invalid("no response"))?;
    let client_data_json = decode(field(response, "clientDataJSON")?)?;
    verify_client_data(&client_data_json, "webauthn.get", challenge, rp)?;
    let auth_data = decode(field(response, "authenticatorData")?)?;
    let signature = decode(field(response, "signature")?)?;

    let data = AuthenticatorData::parse(&auth_data)?;
    data.verify(rp)?;

    let key = CoseKey::parse(&decode(public_key)?)?;
    let mut signed = auth_data.clone();
    signed.extend_from_slice(&Sha256::digest(&client_data_json));
    key.verify(&signed, &signature)?;

    // A counter that does not move forward means a cloned key, unless the
    // authenticator keeps no counter at all.
    let count = i64::from(data.sign_count);
    if (count != 0 || stored_sign_count != 0) && count <= stored_sign_count {
        return invalid("sign count did not increase");
    }
    Ok(count)
}

fn field<'a>(value: &'a Value, name: &'static str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or(Error::Invalid(name))
}

fn verify_client_data(
    raw: &[u8],
    expected_type: &'static str,
    challenge: &str,
    rp: &RelyingParty,
) -> Result<()> {
    let data: Value =
        serde_json::from_slice(raw).or_else(|_| invalid("client data is not JSON"))?;
    if data.get("type").and_then(Value::as_str) != Some(expected_type) {
        return invalid("client data has the wrong type");
    }
    let given = data
        .get("challenge")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid("client data has no challenge"))?;
    if decode(given)? != decode(challenge)? {
        return invalid("challenge does not match");
    }
    if data.get("origin").and_then(Value::as_str) != Some(rp.origin.as_str()) {
        return invalid("origin does not match");
    }
    Ok(())
}

struct AttestedCredential {
    id: Vec<u8>,
    public_key: Vec<u8>,
}

struct AuthenticatorData {
    rp_id_hash: [u8; 32],
    flags: u8,
    sign_count: u32,
    credential: Option<AttestedCredential>,
}

impl AuthenticatorData {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 37 {
            return invalid("authenticator data is too short");
        }
        let mut rp_id_hash = [0u8; 32];
        rp_id_hash.copy_from_slice(&bytes[..32]);
        let flags = bytes[32];
        let sign_count = u32::from_be_bytes([bytes[33], bytes[34], bytes[35], bytes[36]]);
        let credential = if flags & FLAG_ATTESTED_CREDENTIAL != 0 {
            // aaguid (16), credential id length (2), the id, then the key.
            let rest = &bytes[37..];
            if rest.len() < 18 {
                return invalid("attested credential data is too short");
            }
            let id_len = u16::from_be_bytes([rest[16], rest[17]]) as usize;
            let id_end = 18 + id_len;
            if rest.len() < id_end {
                return invalid("credential id overruns the data");
            }
            let id = rest[18..id_end].to_vec();
            let (_, used) = cbor::parse(&rest[id_end..])?;
            Some(AttestedCredential {
                id,
                public_key: rest[id_end..id_end + used].to_vec(),
            })
        } else {
            None
        };
        Ok(Self {
            rp_id_hash,
            flags,
            sign_count,
            credential,
        })
    }

    fn verify(&self, rp: &RelyingParty) -> Result<()> {
        if self.rp_id_hash[..] != Sha256::digest(rp.id.as_bytes())[..] {
            return invalid("relying party does not match");
        }
        if self.flags & FLAG_USER_PRESENT == 0 {
            return invalid("user was not present");
        }
        Ok(())
    }
}

/// A credential public key in COSE form.
struct CoseKey {
    alg: i64,
    material: KeyMaterial,
}

enum KeyMaterial {
    Ec2 { x: Vec<u8>, y: Vec<u8> },
    Rsa { n: Vec<u8>, e: Vec<u8> },
}

impl CoseKey {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let (map, _) = cbor::parse(bytes)?;
        let int = |key: i64| map.get_int(key);
        let alg = int(3)
            .and_then(cbor::Value::as_int)
            .ok_or(Error::Invalid("key has no algorithm"))?;
        let kty = int(1)
            .and_then(cbor::Value::as_int)
            .ok_or(Error::Invalid("key has no type"))?;
        let bytes_at = |key: i64| {
            int(key)
                .and_then(cbor::Value::as_bytes)
                .map(<[u8]>::to_vec)
                .ok_or(Error::Invalid("key is missing a parameter"))
        };
        let material = match (kty, alg) {
            (2, ES256) => {
                if int(-1).and_then(cbor::Value::as_int) != Some(1) {
                    return invalid("only P-256 is supported");
                }
                KeyMaterial::Ec2 {
                    x: bytes_at(-2)?,
                    y: bytes_at(-3)?,
                }
            }
            (3, PS256 | RS256) => KeyMaterial::Rsa {
                n: bytes_at(-1)?,
                e: bytes_at(-2)?,
            },
            _ => return invalid("unsupported key algorithm"),
        };
        Ok(Self { alg, material })
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        match &self.material {
            KeyMaterial::Ec2 { x, y } => {
                use p256::ecdsa::signature::Verifier as _;
                if x.len() != 32 || y.len() != 32 {
                    return invalid("malformed P-256 key");
                }
                let mut point = vec![0x04];
                point.extend_from_slice(x);
                point.extend_from_slice(y);
                let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(&point)
                    .or_else(|_| invalid("malformed P-256 key"))?;
                let signature = p256::ecdsa::Signature::from_der(signature)
                    .or_else(|_| invalid("malformed signature"))?;
                key.verify(message, &signature)
                    .or_else(|_| invalid("signature does not verify"))
            }
            KeyMaterial::Rsa { n, e } => {
                use rsa::signature::Verifier as _;
                let key = rsa::RsaPublicKey::new(
                    rsa::BigUint::from_bytes_be(n),
                    rsa::BigUint::from_bytes_be(e),
                )
                .or_else(|_| invalid("malformed RSA key"))?;
                let verified = if self.alg == RS256 {
                    let signature = rsa::pkcs1v15::Signature::try_from(signature)
                        .or_else(|_| invalid("malformed signature"))?;
                    rsa::pkcs1v15::VerifyingKey::<Sha256>::new(key)
                        .verify(message, &signature)
                        .is_ok()
                } else {
                    let signature = rsa::pss::Signature::try_from(signature)
                        .or_else(|_| invalid("malformed signature"))?;
                    rsa::pss::VerifyingKey::<Sha256>::new(key)
                        .verify(message, &signature)
                        .is_ok()
                };
                if verified {
                    Ok(())
                } else {
                    invalid("signature does not verify")
                }
            }
        }
    }
}

/// Just enough CBOR (RFC 8949) for attestation objects and COSE keys.
pub mod cbor {
    use super::{invalid, Result};

    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        Int(i64),
        Bytes(Vec<u8>),
        Text(String),
        Array(Vec<Value>),
        Map(Vec<(Value, Value)>),
        Bool(bool),
        Null,
        Tagged(u64, Box<Value>),
    }

    impl Value {
        pub fn as_int(&self) -> Option<i64> {
            match self {
                Self::Int(i) => Some(*i),
                _ => None,
            }
        }

        pub fn as_bytes(&self) -> Option<&[u8]> {
            match self {
                Self::Bytes(b) => Some(b),
                _ => None,
            }
        }

        pub fn as_text(&self) -> Option<&str> {
            match self {
                Self::Text(t) => Some(t),
                _ => None,
            }
        }

        fn get(&self, key: &Value) -> Option<&Value> {
            match self {
                Self::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        pub fn get_text(&self, key: &str) -> Option<&Value> {
            self.get(&Value::Text(key.to_owned()))
        }

        pub fn get_int(&self, key: i64) -> Option<&Value> {
            self.get(&Value::Int(key))
        }
    }

    /// The first value in `bytes`, and how many bytes it took.
    pub fn parse(bytes: &[u8]) -> Result<(Value, usize)> {
        let mut pos = 0;
        let value = item(bytes, &mut pos, 0)?;
        Ok((value, pos))
    }

    fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
        let end = pos.checked_add(n).filter(|end| *end <= bytes.len());
        match end {
            Some(end) => {
                let slice = &bytes[*pos..end];
                *pos = end;
                Ok(slice)
            }
            None => invalid("CBOR ends early"),
        }
    }

    fn argument(bytes: &[u8], pos: &mut usize, info: u8) -> Result<u64> {
        Ok(match info {
            0..=23 => u64::from(info),
            24 => u64::from(take(bytes, pos, 1)?[0]),
            25 => u64::from(u16::from_be_bytes(take(bytes, pos, 2)?.try_into().unwrap())),
            26 => u64::from(u32::from_be_bytes(take(bytes, pos, 4)?.try_into().unwrap())),
            27 => u64::from_be_bytes(take(bytes, pos, 8)?.try_into().unwrap()),
            _ => return invalid("unsupported CBOR length"),
        })
    }

    fn item(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<Value> {
        if depth > 16 {
            return invalid("CBOR nests too deeply");
        }
        let initial = take(bytes, pos, 1)?[0];
        let (major, info) = (initial >> 5, initial & 0x1f);
        let length = |pos: &mut usize| -> Result<usize> {
            let n = argument(bytes, pos, info)?;
            usize::try_from(n)
                .ok()
                .filter(|n| *n <= bytes.len())
                .map_or_else(|| invalid("CBOR length is too large"), Ok)
        };
        Ok(match major {
            0 => Value::Int(
                i64::try_from(argument(bytes, pos, info)?)
                    .or_else(|_| invalid("CBOR integer is too large"))?,
            ),
            1 => Value::Int(
                -1 - i64::try_from(argument(bytes, pos, info)?)
                    .or_else(|_| invalid("CBOR integer is too large"))?,
            ),
            2 => {
                let n = length(pos)?;
                Value::Bytes(take(bytes, pos, n)?.to_vec())
            }
            3 => {
                let n = length(pos)?;
                Value::Text(
                    String::from_utf8(take(bytes, pos, n)?.to_vec())
                        .or_else(|_| invalid("CBOR text is not UTF-8"))?,
                )
            }
            4 => {
                let n = length(pos)?;
                let mut items = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    items.push(item(bytes, pos, depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let n = length(pos)?;
                let mut entries = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let key = item(bytes, pos, depth + 1)?;
                    let value = item(bytes, pos, depth + 1)?;
                    entries.push((key, value));
                }
                Value::Map(entries)
            }
            6 => {
                let tag = argument(bytes, pos, info)?;
                Value::Tagged(tag, Box::new(item(bytes, pos, depth + 1)?))
            }
            7 => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 | 23 => Value::Null,
                _ => return invalid("unsupported CBOR simple value"),
            },
            _ => unreachable!("three bits"),
        })
    }

    /// Encode, for tests and for building keys: definite lengths only.
    pub fn encode(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        write(value, &mut out);
        out
    }

    fn head(major: u8, n: u64, out: &mut Vec<u8>) {
        let major = major << 5;
        if n < 24 {
            out.push(major | n as u8);
        } else if n <= u64::from(u8::MAX) {
            out.push(major | 24);
            out.push(n as u8);
        } else if n <= u64::from(u16::MAX) {
            out.push(major | 25);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else if n <= u64::from(u32::MAX) {
            out.push(major | 26);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        } else {
            out.push(major | 27);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }

    fn write(value: &Value, out: &mut Vec<u8>) {
        match value {
            Value::Int(i) if *i >= 0 => head(0, *i as u64, out),
            Value::Int(i) => head(1, (-1 - *i) as u64, out),
            Value::Bytes(b) => {
                head(2, b.len() as u64, out);
                out.extend_from_slice(b);
            }
            Value::Text(t) => {
                head(3, t.len() as u64, out);
                out.extend_from_slice(t.as_bytes());
            }
            Value::Array(items) => {
                head(4, items.len() as u64, out);
                for item in items {
                    write(item, out);
                }
            }
            Value::Map(entries) => {
                head(5, entries.len() as u64, out);
                for (k, v) in entries {
                    write(k, out);
                    write(v, out);
                }
            }
            Value::Tagged(tag, inner) => {
                head(6, *tag, out);
                write(inner, out);
            }
            Value::Bool(false) => out.push(0xf4),
            Value::Bool(true) => out.push(0xf5),
            Value::Null => out.push(0xf6),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Signer as _, SigningKey};

    fn rp() -> RelyingParty {
        RelyingParty::for_domain("keys.example")
    }

    fn cose_key(key: &SigningKey) -> Vec<u8> {
        let point = key.verifying_key().to_encoded_point(false);
        cbor::encode(&cbor::Value::Map(vec![
            (cbor::Value::Int(1), cbor::Value::Int(2)),
            (cbor::Value::Int(3), cbor::Value::Int(ES256)),
            (cbor::Value::Int(-1), cbor::Value::Int(1)),
            (
                cbor::Value::Int(-2),
                cbor::Value::Bytes(point.x().unwrap().to_vec()),
            ),
            (
                cbor::Value::Int(-3),
                cbor::Value::Bytes(point.y().unwrap().to_vec()),
            ),
        ]))
    }

    fn auth_data(
        rp: &RelyingParty,
        flags: u8,
        count: u32,
        attested: Option<(&[u8], &[u8])>,
    ) -> Vec<u8> {
        let mut out = Sha256::digest(rp.id.as_bytes()).to_vec();
        out.push(flags);
        out.extend_from_slice(&count.to_be_bytes());
        if let Some((id, key)) = attested {
            out.extend_from_slice(&[0u8; 16]);
            out.extend_from_slice(&(id.len() as u16).to_be_bytes());
            out.extend_from_slice(id);
            out.extend_from_slice(key);
        }
        out
    }

    fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({ "type": kind, "challenge": challenge, "origin": origin }))
            .unwrap()
    }

    #[test]
    fn registers_and_signs_in_with_a_p256_key() {
        let rp = rp();
        let key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let id = b"credential-one";
        let challenge = generate_challenge();
        let data = auth_data(&rp, 0x41, 0, Some((id, &cose_key(&key))));
        let attestation = cbor::encode(&cbor::Value::Map(vec![
            (
                cbor::Value::Text("fmt".into()),
                cbor::Value::Text("none".into()),
            ),
            (
                cbor::Value::Text("attStmt".into()),
                cbor::Value::Map(vec![]),
            ),
            (
                cbor::Value::Text("authData".into()),
                cbor::Value::Bytes(data),
            ),
        ]));
        let created = json!({
            "id": encode(id),
            "rawId": encode(id),
            "type": "public-key",
            "response": {
                "clientDataJSON": encode(&client_data("webauthn.create", &challenge, &rp.origin)),
                "attestationObject": encode(&attestation),
            },
        });
        let stored = verify_registration(&created, &challenge, &rp).unwrap();
        assert_eq!(stored.external_id, encode(id));
        assert_eq!(stored.sign_count, 0);

        // A different challenge, origin or relying party is refused.
        assert!(verify_registration(&created, &generate_challenge(), &rp).is_err());
        assert!(verify_registration(
            &created,
            &challenge,
            &RelyingParty::for_domain("other.example")
        )
        .is_err());

        let challenge = generate_challenge();
        let data = auth_data(&rp, 0x01, 5, None);
        let client = client_data("webauthn.get", &challenge, &rp.origin);
        let mut signed = data.clone();
        signed.extend_from_slice(&Sha256::digest(&client));
        let signature: p256::ecdsa::Signature = key.sign(&signed);
        let assertion = json!({
            "id": encode(id),
            "type": "public-key",
            "response": {
                "clientDataJSON": encode(&client),
                "authenticatorData": encode(&data),
                "signature": encode(signature.to_der().as_bytes()),
            },
        });
        assert_eq!(
            verify_assertion(&assertion, &challenge, &rp, &stored.public_key, 0).unwrap(),
            5
        );
        // A counter that went backwards means a cloned key.
        assert!(verify_assertion(&assertion, &challenge, &rp, &stored.public_key, 5).is_err());
        // Someone else's key does not verify it.
        let other = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        assert!(
            verify_assertion(&assertion, &challenge, &rp, &encode(&cose_key(&other)), 0).is_err()
        );
    }

    #[test]
    fn cbor_round_trips() {
        let value = cbor::Value::Map(vec![
            (cbor::Value::Int(-257), cbor::Value::Bytes(vec![1; 300])),
            (
                cbor::Value::Text("k".into()),
                cbor::Value::Array(vec![cbor::Value::Bool(true)]),
            ),
        ]);
        let bytes = cbor::encode(&value);
        assert_eq!(cbor::parse(&bytes).unwrap(), (value, bytes.len()));
        assert!(cbor::parse(&bytes[..bytes.len() - 1]).is_err());
    }
}
