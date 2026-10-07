//! `crypto.subtle` (<https://w3c.github.io/webcrypto/>): keys and the
//! operations on them, over the RustCrypto crates.
//!
//! Algorithms: SHA-1/256/384/512 digests; HMAC; AES-GCM, AES-CBC and
//! AES-CTR; PBKDF2 and HKDF; ECDSA and ECDH on P-256 and P-384;
//! RSASSA-PKCS1-v1_5, RSA-PSS and RSA-OAEP; Ed25519 and X25519. Key
//! formats: raw, jwk, pkcs8 and spki. Not offered: AES-KW, wrapKey and
//! unwrapKey, P-521, GCM tag lengths other than 128 bits.
//!
//! Every operation runs to completion on the page thread and settles its
//! promise at once; the spec's asynchrony is only in the promise.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use catpaw_js::{Exception, Fallible, ObjectId, PromiseRef, Value};
use cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipher};
use hmac::Mac;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

use crate::crypto::{SubtleCryptoObject, algorithm_name, fill_random};
use crate::generated as web;
use crate::page::Cx;
use crate::{Web, platform_object};

type Result<T> = std::result::Result<T, Exception>;

fn not_supported(what: impl std::fmt::Display) -> Exception {
    Exception::not_supported(format!("{what}"))
}

fn operation(what: impl std::fmt::Display) -> Exception {
    Exception::dom("OperationError", format!("{what}"))
}

fn data_error(what: impl std::fmt::Display) -> Exception {
    Exception::dom("DataError", format!("{what}"))
}

fn invalid_access(what: impl std::fmt::Display) -> Exception {
    Exception::dom("InvalidAccessError", format!("{what}"))
}

// ------------------------------------------------------------------ hashes

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name.to_ascii_uppercase().as_str() {
            "SHA-1" => Hash::Sha1,
            "SHA-256" => Hash::Sha256,
            "SHA-384" => Hash::Sha384,
            "SHA-512" => Hash::Sha512,
            other => {
                return Err(not_supported(format!(
                    "the hash `{other}` is not supported"
                )));
            }
        })
    }

    fn name(self) -> &'static str {
        match self {
            Hash::Sha1 => "SHA-1",
            Hash::Sha256 => "SHA-256",
            Hash::Sha384 => "SHA-384",
            Hash::Sha512 => "SHA-512",
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => Sha1::digest(data).to_vec(),
            Hash::Sha256 => Sha256::digest(data).to_vec(),
            Hash::Sha384 => Sha384::digest(data).to_vec(),
            Hash::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    /// The block size in bits: HMAC's default key length.
    fn block_bits(self) -> u32 {
        match self {
            Hash::Sha1 | Hash::Sha256 => 512,
            Hash::Sha384 | Hash::Sha512 => 1024,
        }
    }

    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        macro_rules! mac {
            ($d:ty) => {{
                let mut m = <hmac::Hmac<$d> as Mac>::new_from_slice(key).expect("any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }};
        }
        match self {
            Hash::Sha1 => mac!(Sha1),
            Hash::Sha256 => mac!(Sha256),
            Hash::Sha384 => mac!(Sha384),
            Hash::Sha512 => mac!(Sha512),
        }
    }

    fn pbkdf2(self, password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
        match self {
            Hash::Sha1 => pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, iterations, out),
            Hash::Sha256 => pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, out),
            Hash::Sha384 => pbkdf2::pbkdf2_hmac::<Sha384>(password, salt, iterations, out),
            Hash::Sha512 => pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, iterations, out),
        }
    }

    fn hkdf(self, ikm: &[u8], salt: &[u8], info: &[u8], out: &mut [u8]) -> Result<()> {
        macro_rules! expand {
            ($d:ty) => {
                hkdf::Hkdf::<$d>::new(Some(salt), ikm)
                    .expand(info, out)
                    .map_err(|_| operation("the HKDF output is too long"))
            };
        }
        match self {
            Hash::Sha1 => expand!(Sha1),
            Hash::Sha256 => expand!(Sha256),
            Hash::Sha384 => expand!(Sha384),
            Hash::Sha512 => expand!(Sha512),
        }
    }

    /// The JWK algorithm suffix (`HS256`, `RS256`, ...).
    fn jwk_suffix(self) -> &'static str {
        match self {
            Hash::Sha1 => "1",
            Hash::Sha256 => "256",
            Hash::Sha384 => "384",
            Hash::Sha512 => "512",
        }
    }
}

// -------------------------------------------------------------------- keys

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Curve {
    P256,
    P384,
}

impl Curve {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "P-256" => Ok(Curve::P256),
            "P-384" => Ok(Curve::P384),
            other => Err(not_supported(format!(
                "the curve `{other}` is not supported"
            ))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Curve::P256 => "P-256",
            Curve::P384 => "P-384",
        }
    }

    fn field_bytes(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
        }
    }
}

/// What a key holds.
#[derive(Clone)]
enum Material {
    Secret(Vec<u8>),
    /// The scalar, big-endian, field size long.
    EcPrivate {
        curve: Curve,
        scalar: Vec<u8>,
    },
    /// The point, SEC1 uncompressed (`04 || x || y`).
    EcPublic {
        curve: Curve,
        point: Vec<u8>,
    },
    RsaPrivate(Box<rsa::RsaPrivateKey>),
    RsaPublic(rsa::RsaPublicKey),
    EdPrivate([u8; 32]),
    EdPublic([u8; 32]),
    XPrivate([u8; 32]),
    XPublic([u8; 32]),
}

/// A key's algorithm as `key.algorithm` describes it.
#[derive(Clone, Debug)]
struct Algorithm {
    name: String,
    hash: Option<Hash>,
    length: Option<u32>,
    curve: Option<Curve>,
    modulus_length: Option<u32>,
    public_exponent: Option<Vec<u8>>,
}

impl Algorithm {
    fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            hash: None,
            length: None,
            curve: None,
            modulus_length: None,
            public_exponent: None,
        }
    }

    fn to_value(&self) -> Value {
        let mut fields = vec![("name".to_string(), Value::String(self.name.clone()))];
        if let Some(hash) = self.hash {
            fields.push((
                "hash".to_string(),
                Value::Record(vec![(
                    "name".to_string(),
                    Value::String(hash.name().to_string()),
                )]),
            ));
        }
        if let Some(length) = self.length {
            fields.push(("length".to_string(), Value::Number(f64::from(length))));
        }
        if let Some(curve) = self.curve {
            fields.push((
                "namedCurve".to_string(),
                Value::String(curve.name().to_string()),
            ));
        }
        if let Some(bits) = self.modulus_length {
            fields.push(("modulusLength".to_string(), Value::Number(f64::from(bits))));
        }
        if let Some(e) = &self.public_exponent {
            fields.push(("publicExponent".to_string(), Value::Uint8Array(e.clone())));
        }
        Value::Record(fields)
    }
}

pub struct CryptoKeyObject {
    kind: &'static str,
    extractable: bool,
    algorithm: Algorithm,
    usages: Vec<String>,
    material: Material,
}
platform_object!(CryptoKeyObject, CryptoKey);

impl web::CryptoKeyImpl for Web {
    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page
            .with::<CryptoKeyObject, _>(this, |k| k.kind.to_string())
    }

    fn extractable(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        cx.page.with::<CryptoKeyObject, _>(this, |k| k.extractable)
    }

    fn algorithm(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        cx.page
            .with::<CryptoKeyObject, _>(this, |k| k.algorithm.to_value())
    }

    fn usages(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        cx.page.with::<CryptoKeyObject, _>(this, |k| {
            Value::Array(k.usages.iter().map(|u| Value::String(u.clone())).collect())
        })
    }
}

fn key_of(cx: &Cx<'_>, id: ObjectId) -> Result<KeySnapshot> {
    cx.page.with::<CryptoKeyObject, _>(id, |k| KeySnapshot {
        extractable: k.extractable,
        algorithm: k.algorithm.clone(),
        usages: k.usages.clone(),
        material: k.material.clone(),
    })
}

/// A key's state, copied out of the arena for an operation.
#[derive(Clone)]
struct KeySnapshot {
    extractable: bool,
    algorithm: Algorithm,
    usages: Vec<String>,
    material: Material,
}

impl KeySnapshot {
    fn require_usage(&self, usage: &str) -> Result<()> {
        if self.usages.iter().any(|u| u == usage) {
            Ok(())
        } else {
            Err(invalid_access(format!("the key does not allow `{usage}`")))
        }
    }

    fn require_algorithm(&self, name: &str) -> Result<()> {
        if self.algorithm.name.eq_ignore_ascii_case(name) {
            Ok(())
        } else {
            Err(invalid_access(format!(
                "the key is for {}, not {name}",
                self.algorithm.name
            )))
        }
    }
}

fn make_key(
    cx: &Cx<'_>,
    kind: &'static str,
    extractable: bool,
    algorithm: Algorithm,
    usages: Vec<String>,
    material: Material,
) -> ObjectId {
    cx.page.alloc(CryptoKeyObject {
        kind,
        extractable,
        algorithm,
        usages,
        material,
    })
}

/// Checks `usages` against what the algorithm allows for the key kind,
/// keeping the spec's order and dropping duplicates.
fn check_usages(usages: &[String], allowed: &[&str]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for usage in usages {
        if !allowed.contains(&usage.as_str()) {
            return Err(Exception::dom(
                "SyntaxError",
                format!("the usage `{usage}` is not allowed for this key"),
            ));
        }
        if !out.contains(usage) {
            out.push(usage.clone());
        }
    }
    Ok(out)
}

// -------------------------------------------------------------- parameters

/// An algorithm's parameters: its name and the object that carries the
/// rest (a string carries nothing).
struct Params {
    name: String,
    object: Option<Value>,
}

impl Params {
    fn read(cx: &mut Cx<'_>, algorithm: &Value) -> Result<Self> {
        let name = algorithm_name(cx, algorithm)?;
        let object = match algorithm {
            Value::String(_) => None,
            other => Some(other.clone()),
        };
        Ok(Self { name, object })
    }

    fn get(&self, cx: &mut Cx<'_>, field: &str) -> Option<Value> {
        let object = self.object.as_ref()?;
        match cx.script.get_property(object, field) {
            Ok(Value::Undefined) | Err(_) => None,
            Ok(value) => Some(value),
        }
    }

    fn string(&self, cx: &mut Cx<'_>, field: &str) -> Result<Option<String>> {
        match self.get(cx, field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => Ok(Some(cx.script.to_dom_string(&value)?)),
        }
    }

    fn number(&self, cx: &mut Cx<'_>, field: &str) -> Option<f64> {
        match self.get(cx, field)? {
            Value::Number(n) => Some(n),
            Value::String(s) => s.parse().ok(),
            Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    fn bytes(&self, cx: &mut Cx<'_>, field: &str) -> Option<Vec<u8>> {
        let value = self.get(cx, field)?;
        cx.script.buffer_bytes(&value)
    }

    fn required_bytes(&self, cx: &mut Cx<'_>, field: &str) -> Result<Vec<u8>> {
        self.bytes(cx, field).ok_or_else(|| {
            Exception::type_error(format!("`{field}` is required and must be a buffer"))
        })
    }

    /// The `hash` member: a name or an object with one.
    fn hash(&self, cx: &mut Cx<'_>) -> Result<Option<Hash>> {
        let Some(value) = self.get(cx, "hash") else {
            return Ok(None);
        };
        let name = algorithm_name(cx, &value)?;
        Ok(Some(Hash::parse(&name)?))
    }

    fn required_hash(&self, cx: &mut Cx<'_>) -> Result<Hash> {
        self.hash(cx)?
            .ok_or_else(|| Exception::type_error("`hash` is required"))
    }

    /// A `CryptoKey` member (`public` for key agreement).
    fn key(&self, cx: &mut Cx<'_>, field: &str) -> Result<KeySnapshot> {
        match self.get(cx, field) {
            Some(Value::Object(id)) => key_of(cx, id),
            _ => Err(Exception::type_error(format!(
                "`{field}` must be a CryptoKey"
            ))),
        }
    }
}

// ------------------------------------------------------------------ base64

fn b64(bytes: &[u8]) -> Value {
    Value::String(URL_SAFE_NO_PAD.encode(bytes))
}

fn unb64(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .map_err(|_| data_error("a JWK member is not base64url"))
}

/// The members of a JWK, read from the object script passed.
struct Jwk {
    object: Value,
}

impl Jwk {
    fn string(&self, cx: &mut Cx<'_>, field: &str) -> Option<String> {
        match cx.script.get_property(&self.object, field) {
            Ok(Value::String(s)) => Some(s),
            _ => None,
        }
    }

    fn bytes(&self, cx: &mut Cx<'_>, field: &str) -> Result<Option<Vec<u8>>> {
        self.string(cx, field).map(|s| unb64(&s)).transpose()
    }

    fn required(&self, cx: &mut Cx<'_>, field: &str) -> Result<Vec<u8>> {
        self.bytes(cx, field)?
            .ok_or_else(|| data_error(format!("the JWK lacks `{field}`")))
    }

    fn bool(&self, cx: &mut Cx<'_>, field: &str) -> Option<bool> {
        match cx.script.get_property(&self.object, field) {
            Ok(Value::Bool(b)) => Some(b),
            _ => None,
        }
    }
}

fn jwk_record(fields: Vec<(&str, Value)>) -> Value {
    Value::Record(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

// ------------------------------------------------------------------- DER

/// A minimal DER writer for the OKP key formats, whose crates do not
/// offer PKCS#8.
fn der_len(len: usize) -> Vec<u8> {
    if len < 128 {
        vec![len as u8]
    } else if len < 256 {
        vec![0x81, len as u8]
    } else {
        vec![0x82, (len >> 8) as u8, len as u8]
    }
}

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(der_len(body.len()));
    out.extend_from_slice(body);
    out
}

/// `AlgorithmIdentifier` of an OKP curve: Ed25519 is 1.3.101.112, X25519
/// is 1.3.101.110.
fn okp_algorithm(x25519: bool) -> Vec<u8> {
    der(
        0x30,
        &der(0x06, &[0x2b, 0x65, if x25519 { 0x6e } else { 0x70 }]),
    )
}

fn okp_pkcs8(x25519: bool, secret: &[u8; 32]) -> Vec<u8> {
    let mut body = der(0x02, &[0]);
    body.extend(okp_algorithm(x25519));
    body.extend(der(0x04, &der(0x04, secret)));
    der(0x30, &body)
}

fn okp_spki(x25519: bool, public: &[u8; 32]) -> Vec<u8> {
    let mut body = okp_algorithm(x25519);
    let mut bits = vec![0u8];
    bits.extend_from_slice(public);
    body.extend(der(0x03, &bits));
    der(0x30, &body)
}

/// Reads the 32 key bytes out of an OKP PKCS#8 or SPKI document, checking
/// the curve.
fn okp_from_der(x25519: bool, der_bytes: &[u8], private: bool) -> Result<[u8; 32]> {
    let oid: &[u8] = &[0x06, 0x03, 0x2b, 0x65, if x25519 { 0x6e } else { 0x70 }];
    let position = der_bytes
        .windows(oid.len())
        .position(|w| w == oid)
        .ok_or_else(|| data_error("the key is not for this curve"))?;
    let rest = &der_bytes[position + oid.len()..];
    // The key bytes are the last 32 of the document, after the tags.
    let marker: &[u8] = if private {
        &[0x04, 0x22, 0x04, 0x20]
    } else {
        &[0x03, 0x21, 0x00]
    };
    let at = rest
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or_else(|| data_error("the key document is not understood"))?;
    let key = &rest[at + marker.len()..];
    if key.len() != 32 {
        return Err(data_error("the key document is not understood"));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(key);
    Ok(out)
}

// ------------------------------------------------------------ EC helpers

mod ec {
    use super::{Curve, Result, data_error, operation};
    use ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
    use p256::elliptic_curve::sec1::ToEncodedPoint as _;

    pub fn generate(curve: Curve) -> (Vec<u8>, Vec<u8>) {
        let mut rng = rand::rngs::OsRng;
        match curve {
            Curve::P256 => {
                let secret = p256::SecretKey::random(&mut rng);
                let point = secret.public_key().to_encoded_point(false);
                (secret.to_bytes().to_vec(), point.as_bytes().to_vec())
            }
            Curve::P384 => {
                let secret = p384::SecretKey::random(&mut rng);
                let point = secret.public_key().to_encoded_point(false);
                (secret.to_bytes().to_vec(), point.as_bytes().to_vec())
            }
        }
    }

    /// The public point of a scalar.
    pub fn public_of(curve: Curve, scalar: &[u8]) -> Result<Vec<u8>> {
        Ok(match curve {
            Curve::P256 => p256::SecretKey::from_slice(scalar)
                .map_err(|_| data_error("the private key is not valid"))?
                .public_key()
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::P384 => p384::SecretKey::from_slice(scalar)
                .map_err(|_| data_error("the private key is not valid"))?
                .public_key()
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
        })
    }

    /// Validates a point and returns it uncompressed.
    pub fn normalize_point(curve: Curve, point: &[u8]) -> Result<Vec<u8>> {
        Ok(match curve {
            Curve::P256 => p256::PublicKey::from_sec1_bytes(point)
                .map_err(|_| data_error("the public key is not a point on the curve"))?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::P384 => p384::PublicKey::from_sec1_bytes(point)
                .map_err(|_| data_error("the public key is not a point on the curve"))?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
        })
    }

    /// The x and y coordinates of an uncompressed point.
    pub fn coordinates(curve: Curve, point: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let n = curve.field_bytes();
        (point[1..1 + n].to_vec(), point[1 + n..1 + 2 * n].to_vec())
    }

    pub fn sign(curve: Curve, scalar: &[u8], digest: &[u8]) -> Result<Vec<u8>> {
        Ok(match curve {
            Curve::P256 => {
                let key = p256::ecdsa::SigningKey::from_slice(scalar)
                    .map_err(|_| operation("the signing key is not valid"))?;
                let signature: p256::ecdsa::Signature = key
                    .sign_prehash(digest)
                    .map_err(|e| operation(format!("signing failed: {e}")))?;
                signature.to_bytes().to_vec()
            }
            Curve::P384 => {
                let key = p384::ecdsa::SigningKey::from_slice(scalar)
                    .map_err(|_| operation("the signing key is not valid"))?;
                let signature: p384::ecdsa::Signature = key
                    .sign_prehash(digest)
                    .map_err(|e| operation(format!("signing failed: {e}")))?;
                signature.to_bytes().to_vec()
            }
        })
    }

    pub fn verify(curve: Curve, point: &[u8], digest: &[u8], signature: &[u8]) -> bool {
        match curve {
            Curve::P256 => {
                let Ok(key) = p256::ecdsa::VerifyingKey::from_sec1_bytes(point) else {
                    return false;
                };
                let Ok(signature) = p256::ecdsa::Signature::from_slice(signature) else {
                    return false;
                };
                key.verify_prehash(digest, &signature).is_ok()
            }
            Curve::P384 => {
                let Ok(key) = p384::ecdsa::VerifyingKey::from_sec1_bytes(point) else {
                    return false;
                };
                let Ok(signature) = p384::ecdsa::Signature::from_slice(signature) else {
                    return false;
                };
                key.verify_prehash(digest, &signature).is_ok()
            }
        }
    }

    pub fn agree(curve: Curve, scalar: &[u8], point: &[u8]) -> Result<Vec<u8>> {
        Ok(match curve {
            Curve::P256 => {
                let secret = p256::SecretKey::from_slice(scalar)
                    .map_err(|_| operation("the private key is not valid"))?;
                let public = p256::PublicKey::from_sec1_bytes(point)
                    .map_err(|_| operation("the public key is not valid"))?;
                p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), public.as_affine())
                    .raw_secret_bytes()
                    .to_vec()
            }
            Curve::P384 => {
                let secret = p384::SecretKey::from_slice(scalar)
                    .map_err(|_| operation("the private key is not valid"))?;
                let public = p384::PublicKey::from_sec1_bytes(point)
                    .map_err(|_| operation("the public key is not valid"))?;
                p384::ecdh::diffie_hellman(secret.to_nonzero_scalar(), public.as_affine())
                    .raw_secret_bytes()
                    .to_vec()
            }
        })
    }

    pub fn private_pkcs8(curve: Curve, scalar: &[u8]) -> Result<Vec<u8>> {
        use p256::pkcs8::EncodePrivateKey as _;
        Ok(match curve {
            Curve::P256 => p256::SecretKey::from_slice(scalar)
                .map_err(|_| operation("the private key is not valid"))?
                .to_pkcs8_der()
                .map_err(|e| operation(format!("encoding failed: {e}")))?
                .as_bytes()
                .to_vec(),
            Curve::P384 => p384::SecretKey::from_slice(scalar)
                .map_err(|_| operation("the private key is not valid"))?
                .to_pkcs8_der()
                .map_err(|e| operation(format!("encoding failed: {e}")))?
                .as_bytes()
                .to_vec(),
        })
    }

    pub fn private_from_pkcs8(curve: Curve, der: &[u8]) -> Result<Vec<u8>> {
        use p256::pkcs8::DecodePrivateKey as _;
        Ok(match curve {
            Curve::P256 => p256::SecretKey::from_pkcs8_der(der)
                .map_err(|_| data_error("the PKCS#8 key is not a P-256 key"))?
                .to_bytes()
                .to_vec(),
            Curve::P384 => p384::SecretKey::from_pkcs8_der(der)
                .map_err(|_| data_error("the PKCS#8 key is not a P-384 key"))?
                .to_bytes()
                .to_vec(),
        })
    }

    pub fn public_spki(curve: Curve, point: &[u8]) -> Result<Vec<u8>> {
        use p256::pkcs8::EncodePublicKey as _;
        Ok(match curve {
            Curve::P256 => p256::PublicKey::from_sec1_bytes(point)
                .map_err(|_| operation("the public key is not valid"))?
                .to_public_key_der()
                .map_err(|e| operation(format!("encoding failed: {e}")))?
                .as_bytes()
                .to_vec(),
            Curve::P384 => p384::PublicKey::from_sec1_bytes(point)
                .map_err(|_| operation("the public key is not valid"))?
                .to_public_key_der()
                .map_err(|e| operation(format!("encoding failed: {e}")))?
                .as_bytes()
                .to_vec(),
        })
    }

    pub fn public_from_spki(curve: Curve, der: &[u8]) -> Result<Vec<u8>> {
        use p256::pkcs8::DecodePublicKey as _;
        Ok(match curve {
            Curve::P256 => p256::PublicKey::from_public_key_der(der)
                .map_err(|_| data_error("the SPKI key is not a P-256 key"))?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
            Curve::P384 => p384::PublicKey::from_public_key_der(der)
                .map_err(|_| data_error("the SPKI key is not a P-384 key"))?
                .to_encoded_point(false)
                .as_bytes()
                .to_vec(),
        })
    }
}

// ----------------------------------------------------------- RSA helpers

mod rsa_ops {
    use super::{Hash, Result, data_error, operation};
    use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
    use rsa::traits::{PrivateKeyParts, PublicKeyParts};
    use rsa::{BigUint, Oaep, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};
    use sha1::Sha1;
    use sha2::{Sha256, Sha384, Sha512};

    pub fn generate(bits: usize, exponent: &[u8]) -> Result<RsaPrivateKey> {
        let e = BigUint::from_bytes_be(exponent);
        let mut rng = rand::rngs::OsRng;
        RsaPrivateKey::new_with_exp(&mut rng, bits, &e)
            .map_err(|e| operation(format!("RSA key generation failed: {e}")))
    }

    pub fn pkcs1v15_sign(key: &RsaPrivateKey, hash: Hash, digest: &[u8]) -> Result<Vec<u8>> {
        let scheme = match hash {
            Hash::Sha1 => Pkcs1v15Sign::new::<Sha1>(),
            Hash::Sha256 => Pkcs1v15Sign::new::<Sha256>(),
            Hash::Sha384 => Pkcs1v15Sign::new::<Sha384>(),
            Hash::Sha512 => Pkcs1v15Sign::new::<Sha512>(),
        };
        key.sign(scheme, digest)
            .map_err(|e| operation(format!("signing failed: {e}")))
    }

    pub fn pkcs1v15_verify(
        key: &RsaPublicKey,
        hash: Hash,
        digest: &[u8],
        signature: &[u8],
    ) -> bool {
        let scheme = match hash {
            Hash::Sha1 => Pkcs1v15Sign::new::<Sha1>(),
            Hash::Sha256 => Pkcs1v15Sign::new::<Sha256>(),
            Hash::Sha384 => Pkcs1v15Sign::new::<Sha384>(),
            Hash::Sha512 => Pkcs1v15Sign::new::<Sha512>(),
        };
        key.verify(scheme, digest, signature).is_ok()
    }

    pub fn pss_sign(
        key: &RsaPrivateKey,
        hash: Hash,
        salt: usize,
        digest: &[u8],
    ) -> Result<Vec<u8>> {
        let scheme = match hash {
            Hash::Sha1 => Pss::new_with_salt::<Sha1>(salt),
            Hash::Sha256 => Pss::new_with_salt::<Sha256>(salt),
            Hash::Sha384 => Pss::new_with_salt::<Sha384>(salt),
            Hash::Sha512 => Pss::new_with_salt::<Sha512>(salt),
        };
        let mut rng = rand::rngs::OsRng;
        key.sign_with_rng(&mut rng, scheme, digest)
            .map_err(|e| operation(format!("signing failed: {e}")))
    }

    pub fn pss_verify(
        key: &RsaPublicKey,
        hash: Hash,
        salt: usize,
        digest: &[u8],
        signature: &[u8],
    ) -> bool {
        let scheme = match hash {
            Hash::Sha1 => Pss::new_with_salt::<Sha1>(salt),
            Hash::Sha256 => Pss::new_with_salt::<Sha256>(salt),
            Hash::Sha384 => Pss::new_with_salt::<Sha384>(salt),
            Hash::Sha512 => Pss::new_with_salt::<Sha512>(salt),
        };
        key.verify(scheme, digest, signature).is_ok()
    }

    fn oaep(hash: Hash, label: Option<Vec<u8>>) -> Oaep {
        let mut scheme = match hash {
            Hash::Sha1 => Oaep::new::<Sha1>(),
            Hash::Sha256 => Oaep::new::<Sha256>(),
            Hash::Sha384 => Oaep::new::<Sha384>(),
            Hash::Sha512 => Oaep::new::<Sha512>(),
        };
        if let Some(label) = label
            && !label.is_empty()
        {
            scheme.label = Some(String::from_utf8_lossy(&label).into_owned());
        }
        scheme
    }

    pub fn oaep_encrypt(
        key: &RsaPublicKey,
        hash: Hash,
        label: Option<Vec<u8>>,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let mut rng = rand::rngs::OsRng;
        key.encrypt(&mut rng, oaep(hash, label), data)
            .map_err(|e| operation(format!("encryption failed: {e}")))
    }

    pub fn oaep_decrypt(
        key: &RsaPrivateKey,
        hash: Hash,
        label: Option<Vec<u8>>,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        key.decrypt(oaep(hash, label), data)
            .map_err(|_| operation("decryption failed"))
    }

    pub fn private_pkcs8(key: &RsaPrivateKey) -> Result<Vec<u8>> {
        Ok(key
            .to_pkcs8_der()
            .map_err(|e| operation(format!("encoding failed: {e}")))?
            .as_bytes()
            .to_vec())
    }

    pub fn private_from_pkcs8(der: &[u8]) -> Result<RsaPrivateKey> {
        RsaPrivateKey::from_pkcs8_der(der)
            .map_err(|_| data_error("the PKCS#8 key is not an RSA key"))
    }

    pub fn public_spki(key: &RsaPublicKey) -> Result<Vec<u8>> {
        Ok(key
            .to_public_key_der()
            .map_err(|e| operation(format!("encoding failed: {e}")))?
            .as_bytes()
            .to_vec())
    }

    pub fn public_from_spki(der: &[u8]) -> Result<RsaPublicKey> {
        RsaPublicKey::from_public_key_der(der)
            .map_err(|_| data_error("the SPKI key is not an RSA key"))
    }

    pub fn public_jwk(key: &RsaPublicKey) -> (Vec<u8>, Vec<u8>) {
        (key.n().to_bytes_be(), key.e().to_bytes_be())
    }

    /// `n, e, d, p, q, dp, dq, qi`.
    pub fn private_jwk(key: &RsaPrivateKey) -> Vec<Vec<u8>> {
        let primes = key.primes();
        let (p, q) = (primes[0].clone(), primes[1].clone());
        let dp = key.dp().cloned().unwrap_or_else(|| key.d() % (&p - 1u32));
        let dq = key.dq().cloned().unwrap_or_else(|| key.d() % (&q - 1u32));
        let qi = key
            .qinv()
            .map(|qi| qi.to_biguint().unwrap_or_default())
            .unwrap_or_default();
        vec![
            key.n().to_bytes_be(),
            key.e().to_bytes_be(),
            key.d().to_bytes_be(),
            p.to_bytes_be(),
            q.to_bytes_be(),
            dp.to_bytes_be(),
            dq.to_bytes_be(),
            qi.to_bytes_be(),
        ]
    }

    pub fn private_from_parts(
        n: &[u8],
        e: &[u8],
        d: &[u8],
        p: &[u8],
        q: &[u8],
    ) -> Result<RsaPrivateKey> {
        RsaPrivateKey::from_components(
            BigUint::from_bytes_be(n),
            BigUint::from_bytes_be(e),
            BigUint::from_bytes_be(d),
            vec![BigUint::from_bytes_be(p), BigUint::from_bytes_be(q)],
        )
        .map_err(|_| data_error("the RSA JWK is not consistent"))
    }

    pub fn public_from_parts(n: &[u8], e: &[u8]) -> Result<RsaPublicKey> {
        RsaPublicKey::new(BigUint::from_bytes_be(n), BigUint::from_bytes_be(e))
            .map_err(|_| data_error("the RSA JWK is not valid"))
    }

    pub fn modulus_bits(key: &RsaPublicKey) -> u32 {
        (key.size() * 8) as u32
    }

    pub fn exponent(key: &RsaPublicKey) -> Vec<u8> {
        key.e().to_bytes_be()
    }
}

// ----------------------------------------------------------- AES helpers

fn aes_key_length(key: &[u8]) -> Result<()> {
    if matches!(key.len(), 16 | 24 | 32) {
        Ok(())
    } else {
        Err(data_error("an AES key must be 128, 192 or 256 bits"))
    }
}

fn aes_gcm(key: &[u8], iv: &[u8], aad: &[u8], data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    if iv.len() != 12 {
        return Err(not_supported("only 96-bit AES-GCM IVs are supported"));
    }
    let nonce = aes_gcm::Nonce::from_slice(iv);
    let payload = Payload { msg: data, aad };
    macro_rules! run {
        ($cipher:ty) => {{
            let cipher = <$cipher>::new_from_slice(key).map_err(|_| operation("bad key"))?;
            if encrypt {
                cipher.encrypt(nonce, payload)
            } else {
                cipher.decrypt(nonce, payload)
            }
            .map_err(|_| operation("AES-GCM failed: the data or tag is wrong"))
        }};
    }
    match key.len() {
        16 => run!(aes_gcm::Aes128Gcm),
        24 => run!(aes_gcm::AesGcm<aes::Aes192, aes_gcm::aead::consts::U12>),
        32 => run!(aes_gcm::Aes256Gcm),
        _ => Err(operation("bad key length")),
    }
}

fn aes_cbc(key: &[u8], iv: &[u8], data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use cipher::block_padding::NoPadding;
    if iv.len() != 16 {
        return Err(operation("an AES-CBC IV must be 16 bytes"));
    }
    if encrypt {
        // PKCS#7 padding, always at least one byte.
        let pad = 16 - data.len() % 16;
        let mut buf = data.to_vec();
        buf.extend(std::iter::repeat_n(pad as u8, pad));
        let len = buf.len();
        macro_rules! run {
            ($c:ty) => {{
                let enc = cbc::Encryptor::<$c>::new_from_slices(key, iv)
                    .map_err(|_| operation("bad key"))?;
                enc.encrypt_padded_mut::<NoPadding>(&mut buf, len)
                    .map_err(|_| operation("encryption failed"))?;
            }};
        }
        match key.len() {
            16 => run!(aes::Aes128),
            24 => run!(aes::Aes192),
            32 => run!(aes::Aes256),
            _ => return Err(operation("bad key length")),
        }
        Ok(buf)
    } else {
        if data.is_empty() || !data.len().is_multiple_of(16) {
            return Err(operation("the data is not a whole number of blocks"));
        }
        let mut buf = data.to_vec();
        macro_rules! run {
            ($c:ty) => {{
                let dec = cbc::Decryptor::<$c>::new_from_slices(key, iv)
                    .map_err(|_| operation("bad key"))?;
                dec.decrypt_padded_mut::<NoPadding>(&mut buf)
                    .map_err(|_| operation("decryption failed"))?;
            }};
        }
        match key.len() {
            16 => run!(aes::Aes128),
            24 => run!(aes::Aes192),
            32 => run!(aes::Aes256),
            _ => return Err(operation("bad key length")),
        }
        let pad = *buf.last().unwrap_or(&0) as usize;
        if pad == 0
            || pad > 16
            || pad > buf.len()
            || buf[buf.len() - pad..].iter().any(|b| *b as usize != pad)
        {
            return Err(operation("the padding is wrong"));
        }
        buf.truncate(buf.len() - pad);
        Ok(buf)
    }
}

fn aes_ctr(key: &[u8], counter: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if counter.len() != 16 {
        return Err(operation("an AES-CTR counter must be 16 bytes"));
    }
    let mut buf = data.to_vec();
    macro_rules! run {
        ($c:ty) => {{
            let mut cipher = ctr::Ctr128BE::<$c>::new_from_slices(key, counter)
                .map_err(|_| operation("bad key"))?;
            cipher.apply_keystream(&mut buf);
        }};
    }
    match key.len() {
        16 => run!(aes::Aes128),
        24 => run!(aes::Aes192),
        32 => run!(aes::Aes256),
        _ => return Err(operation("bad key length")),
    }
    Ok(buf)
}

// -------------------------------------------------------------- the API

fn settle(cx: &mut Cx<'_>, result: Result<Value>) -> PromiseRef {
    let promise = cx.script.new_promise();
    match result {
        Ok(value) => cx.script.resolve_promise(&promise, value),
        Err(e) => cx.script.reject_promise(&promise, e),
    }
    promise
}

fn is_aes(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "AES-GCM" | "AES-CBC" | "AES-CTR"
    )
}

fn canonical(name: &str) -> String {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "RSASSA-PKCS1-V1_5" => "RSASSA-PKCS1-v1_5".to_string(),
        "ED25519" => "Ed25519".to_string(),
        other => other.to_string(),
    }
}

fn generate_key(
    cx: &mut Cx<'_>,
    params: Params,
    extractable: bool,
    usages: Vec<String>,
) -> Result<Value> {
    let name = canonical(&params.name);
    match name.as_str() {
        "HMAC" => {
            let hash = params.required_hash(cx)?;
            let length = params
                .number(cx, "length")
                .map(|n| n as u32)
                .unwrap_or(hash.block_bits());
            if length == 0 || !length.is_multiple_of(8) {
                return Err(operation(
                    "the HMAC key length must be a multiple of 8 bits",
                ));
            }
            let usages = check_usages(&usages, &["sign", "verify"])?;
            let mut key = vec![0u8; (length / 8) as usize];
            fill_random(&mut key)?;
            let algorithm = Algorithm {
                hash: Some(hash),
                length: Some(length),
                ..Algorithm::named("HMAC")
            };
            Ok(Value::Object(make_key(
                cx,
                "secret",
                extractable,
                algorithm,
                usages,
                Material::Secret(key),
            )))
        }
        aes if is_aes(aes) => {
            let length = params
                .number(cx, "length")
                .map(|n| n as u32)
                .ok_or_else(|| Exception::type_error("`length` is required"))?;
            if !matches!(length, 128 | 192 | 256) {
                return Err(operation("an AES key length must be 128, 192 or 256"));
            }
            let usages = check_usages(&usages, &["encrypt", "decrypt", "wrapKey", "unwrapKey"])?;
            let mut key = vec![0u8; (length / 8) as usize];
            fill_random(&mut key)?;
            let algorithm = Algorithm {
                length: Some(length),
                ..Algorithm::named(aes)
            };
            Ok(Value::Object(make_key(
                cx,
                "secret",
                extractable,
                algorithm,
                usages,
                Material::Secret(key),
            )))
        }
        "ECDSA" | "ECDH" => {
            let curve = Curve::parse(
                &params
                    .string(cx, "namedCurve")?
                    .ok_or_else(|| Exception::type_error("`namedCurve` is required"))?,
            )?;
            let (private_usages, public_usages) = if name == "ECDSA" {
                (check_usages(&usages, &["sign", "verify"])?, Vec::new())
            } else {
                (
                    check_usages(&usages, &["deriveKey", "deriveBits"])?,
                    Vec::new(),
                )
            };
            let private_usages: Vec<String> = private_usages
                .into_iter()
                .filter(|u| u != "verify")
                .collect();
            let public_usages: Vec<String> = if name == "ECDSA" {
                usages.iter().filter(|u| *u == "verify").cloned().collect()
            } else {
                public_usages
            };
            let (scalar, point) = ec::generate(curve);
            let algorithm = Algorithm {
                curve: Some(curve),
                ..Algorithm::named(&name)
            };
            let public = make_key(
                cx,
                "public",
                true,
                algorithm.clone(),
                public_usages,
                Material::EcPublic { curve, point },
            );
            let private = make_key(
                cx,
                "private",
                extractable,
                algorithm,
                private_usages,
                Material::EcPrivate { curve, scalar },
            );
            Ok(key_pair(public, private))
        }
        "RSASSA-PKCS1-v1_5" | "RSA-PSS" | "RSA-OAEP" => {
            let hash = params.required_hash(cx)?;
            let bits = params
                .number(cx, "modulusLength")
                .map(|n| n as u32)
                .ok_or_else(|| Exception::type_error("`modulusLength` is required"))?;
            if !(1024..=4096).contains(&bits) || !bits.is_multiple_of(8) {
                return Err(operation(
                    "the modulus length must be between 1024 and 4096 bits",
                ));
            }
            let exponent = params.required_bytes(cx, "publicExponent")?;
            if exponent.iter().all(|b| *b == 0) {
                return Err(operation("the public exponent must not be zero"));
            }
            let allowed: &[&str] = if name == "RSA-OAEP" {
                &["encrypt", "decrypt", "wrapKey", "unwrapKey"]
            } else {
                &["sign", "verify"]
            };
            let usages = check_usages(&usages, allowed)?;
            let key = rsa_ops::generate(bits as usize, &exponent)?;
            let public = rsa::RsaPublicKey::from(&key);
            let algorithm = Algorithm {
                hash: Some(hash),
                modulus_length: Some(bits),
                public_exponent: Some(exponent),
                ..Algorithm::named(&name)
            };
            let (public_usages, private_usages): (Vec<String>, Vec<String>) = usages
                .into_iter()
                .partition(|u| matches!(u.as_str(), "verify" | "encrypt" | "wrapKey"));
            let public = make_key(
                cx,
                "public",
                true,
                algorithm.clone(),
                public_usages,
                Material::RsaPublic(public),
            );
            let private = make_key(
                cx,
                "private",
                extractable,
                algorithm,
                private_usages,
                Material::RsaPrivate(Box::new(key)),
            );
            Ok(key_pair(public, private))
        }
        "Ed25519" => {
            let usages = check_usages(&usages, &["sign", "verify"])?;
            let mut secret = [0u8; 32];
            fill_random(&mut secret)?;
            let public = ed25519_dalek::SigningKey::from_bytes(&secret)
                .verifying_key()
                .to_bytes();
            let algorithm = Algorithm::named("Ed25519");
            let public_usages: Vec<String> =
                usages.iter().filter(|u| *u == "verify").cloned().collect();
            let private_usages: Vec<String> =
                usages.iter().filter(|u| *u == "sign").cloned().collect();
            let public = make_key(
                cx,
                "public",
                true,
                algorithm.clone(),
                public_usages,
                Material::EdPublic(public),
            );
            let private = make_key(
                cx,
                "private",
                extractable,
                algorithm,
                private_usages,
                Material::EdPrivate(secret),
            );
            Ok(key_pair(public, private))
        }
        "X25519" => {
            let usages = check_usages(&usages, &["deriveKey", "deriveBits"])?;
            let mut secret = [0u8; 32];
            fill_random(&mut secret)?;
            let public =
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret)).to_bytes();
            let algorithm = Algorithm::named("X25519");
            let public = make_key(
                cx,
                "public",
                true,
                algorithm.clone(),
                Vec::new(),
                Material::XPublic(public),
            );
            let private = make_key(
                cx,
                "private",
                extractable,
                algorithm,
                usages,
                Material::XPrivate(secret),
            );
            Ok(key_pair(public, private))
        }
        other => Err(not_supported(format!(
            "the algorithm `{other}` is not supported"
        ))),
    }
}

fn key_pair(public: ObjectId, private: ObjectId) -> Value {
    Value::Record(vec![
        ("publicKey".to_string(), Value::Object(public)),
        ("privateKey".to_string(), Value::Object(private)),
    ])
}

fn import_key(
    cx: &mut Cx<'_>,
    format: &str,
    key_data: Value,
    params: Params,
    extractable: bool,
    usages: Vec<String>,
) -> Result<Value> {
    let name = canonical(&params.name);
    let raw_bytes = cx.script.buffer_bytes(&key_data);
    let raw = || -> Result<Vec<u8>> {
        raw_bytes
            .clone()
            .ok_or_else(|| Exception::type_error("the key data must be a buffer"))
    };
    let jwk = Jwk {
        object: key_data.clone(),
    };
    let jwk_matches = |cx: &mut Cx<'_>, kty: &str| -> Result<()> {
        if format != "jwk" {
            return Err(not_supported(format!(
                "the `{format}` format is not supported for this key"
            )));
        }
        if jwk.string(cx, "kty").as_deref() != Some(kty) {
            return Err(data_error(format!("the JWK is not of type `{kty}`")));
        }
        if jwk.bool(cx, "ext") == Some(false) && extractable {
            return Err(data_error("the JWK is not extractable"));
        }
        Ok(())
    };
    match name.as_str() {
        "HMAC" => {
            let hash = params.required_hash(cx)?;
            let usages = check_usages(&usages, &["sign", "verify"])?;
            let bytes = match format {
                "raw" => raw()?,
                _ => {
                    jwk_matches(cx, "oct")?;
                    jwk.required(cx, "k")?
                }
            };
            if bytes.is_empty() {
                return Err(data_error("an HMAC key must not be empty"));
            }
            let algorithm = Algorithm {
                hash: Some(hash),
                length: Some((bytes.len() * 8) as u32),
                ..Algorithm::named("HMAC")
            };
            Ok(Value::Object(make_key(
                cx,
                "secret",
                extractable,
                algorithm,
                usages,
                Material::Secret(bytes),
            )))
        }
        aes if is_aes(aes) => {
            let usages = check_usages(&usages, &["encrypt", "decrypt", "wrapKey", "unwrapKey"])?;
            let bytes = match format {
                "raw" => raw()?,
                _ => {
                    jwk_matches(cx, "oct")?;
                    jwk.required(cx, "k")?
                }
            };
            aes_key_length(&bytes)?;
            let algorithm = Algorithm {
                length: Some((bytes.len() * 8) as u32),
                ..Algorithm::named(aes)
            };
            Ok(Value::Object(make_key(
                cx,
                "secret",
                extractable,
                algorithm,
                usages,
                Material::Secret(bytes),
            )))
        }
        "PBKDF2" | "HKDF" => {
            if format != "raw" {
                return Err(not_supported(format!("{name} keys are imported raw")));
            }
            if extractable {
                return Err(Exception::dom(
                    "SyntaxError",
                    format!("{name} keys cannot be extractable"),
                ));
            }
            let usages = check_usages(&usages, &["deriveKey", "deriveBits"])?;
            let bytes = raw()?;
            Ok(Value::Object(make_key(
                cx,
                "secret",
                false,
                Algorithm::named(&name),
                usages,
                Material::Secret(bytes),
            )))
        }
        "ECDSA" | "ECDH" => {
            let curve = Curve::parse(
                &params
                    .string(cx, "namedCurve")?
                    .ok_or_else(|| Exception::type_error("`namedCurve` is required"))?,
            )?;
            let algorithm = Algorithm {
                curve: Some(curve),
                ..Algorithm::named(&name)
            };
            let (public_allowed, private_allowed): (&[&str], &[&str]) = if name == "ECDSA" {
                (&["verify"], &["sign"])
            } else {
                (&[], &["deriveKey", "deriveBits"])
            };
            match format {
                "raw" => {
                    let point = ec::normalize_point(curve, &raw()?)?;
                    let usages = check_usages(&usages, public_allowed)?;
                    Ok(Value::Object(make_key(
                        cx,
                        "public",
                        extractable,
                        algorithm,
                        usages,
                        Material::EcPublic { curve, point },
                    )))
                }
                "spki" => {
                    let point = ec::public_from_spki(curve, &raw()?)?;
                    let usages = check_usages(&usages, public_allowed)?;
                    Ok(Value::Object(make_key(
                        cx,
                        "public",
                        extractable,
                        algorithm,
                        usages,
                        Material::EcPublic { curve, point },
                    )))
                }
                "pkcs8" => {
                    let scalar = ec::private_from_pkcs8(curve, &raw()?)?;
                    let usages = check_usages(&usages, private_allowed)?;
                    Ok(Value::Object(make_key(
                        cx,
                        "private",
                        extractable,
                        algorithm,
                        usages,
                        Material::EcPrivate { curve, scalar },
                    )))
                }
                _ => {
                    jwk_matches(cx, "EC")?;
                    if jwk.string(cx, "crv").as_deref() != Some(curve.name()) {
                        return Err(data_error("the JWK curve does not match"));
                    }
                    let x = jwk.required(cx, "x")?;
                    let y = jwk.required(cx, "y")?;
                    let mut point = vec![4u8];
                    point.extend(x);
                    point.extend(y);
                    let point = ec::normalize_point(curve, &point)?;
                    match jwk.bytes(cx, "d")? {
                        Some(scalar) => {
                            if ec::public_of(curve, &scalar)? != point {
                                return Err(data_error(
                                    "the JWK private key does not match its public point",
                                ));
                            }
                            let usages = check_usages(&usages, private_allowed)?;
                            Ok(Value::Object(make_key(
                                cx,
                                "private",
                                extractable,
                                algorithm,
                                usages,
                                Material::EcPrivate { curve, scalar },
                            )))
                        }
                        None => {
                            let usages = check_usages(&usages, public_allowed)?;
                            Ok(Value::Object(make_key(
                                cx,
                                "public",
                                extractable,
                                algorithm,
                                usages,
                                Material::EcPublic { curve, point },
                            )))
                        }
                    }
                }
            }
        }
        "RSASSA-PKCS1-v1_5" | "RSA-PSS" | "RSA-OAEP" => {
            let hash = params.required_hash(cx)?;
            let (public_allowed, private_allowed): (&[&str], &[&str]) = if name == "RSA-OAEP" {
                (&["encrypt", "wrapKey"], &["decrypt", "unwrapKey"])
            } else {
                (&["verify"], &["sign"])
            };
            let finish = |cx: &mut Cx<'_>,
                          kind: &'static str,
                          public: rsa::RsaPublicKey,
                          material: Material,
                          allowed: &[&str]|
             -> Result<Value> {
                let usages = check_usages(&usages, allowed)?;
                let algorithm = Algorithm {
                    hash: Some(hash),
                    modulus_length: Some(rsa_ops::modulus_bits(&public)),
                    public_exponent: Some(rsa_ops::exponent(&public)),
                    ..Algorithm::named(&name)
                };
                Ok(Value::Object(make_key(
                    cx,
                    kind,
                    extractable,
                    algorithm,
                    usages,
                    material,
                )))
            };
            match format {
                "spki" => {
                    let key = rsa_ops::public_from_spki(&raw()?)?;
                    finish(
                        cx,
                        "public",
                        key.clone(),
                        Material::RsaPublic(key),
                        public_allowed,
                    )
                }
                "pkcs8" => {
                    let key = rsa_ops::private_from_pkcs8(&raw()?)?;
                    let public = rsa::RsaPublicKey::from(&key);
                    finish(
                        cx,
                        "private",
                        public,
                        Material::RsaPrivate(Box::new(key)),
                        private_allowed,
                    )
                }
                "raw" => Err(not_supported("RSA keys are not imported raw")),
                _ => {
                    jwk_matches(cx, "RSA")?;
                    let n = jwk.required(cx, "n")?;
                    let e = jwk.required(cx, "e")?;
                    match jwk.bytes(cx, "d")? {
                        Some(d) => {
                            let p = jwk.required(cx, "p")?;
                            let q = jwk.required(cx, "q")?;
                            let key = rsa_ops::private_from_parts(&n, &e, &d, &p, &q)?;
                            let public = rsa::RsaPublicKey::from(&key);
                            finish(
                                cx,
                                "private",
                                public,
                                Material::RsaPrivate(Box::new(key)),
                                private_allowed,
                            )
                        }
                        None => {
                            let key = rsa_ops::public_from_parts(&n, &e)?;
                            finish(
                                cx,
                                "public",
                                key.clone(),
                                Material::RsaPublic(key),
                                public_allowed,
                            )
                        }
                    }
                }
            }
        }
        "Ed25519" | "X25519" => {
            let x25519 = name == "X25519";
            let (public_allowed, private_allowed): (&[&str], &[&str]) = if x25519 {
                (&[], &["deriveKey", "deriveBits"])
            } else {
                (&["verify"], &["sign"])
            };
            let algorithm = Algorithm::named(&name);
            let key32 = |bytes: Vec<u8>| -> Result<[u8; 32]> {
                <[u8; 32]>::try_from(bytes).map_err(|_| data_error("the key must be 32 bytes"))
            };
            let public_material = |bytes: [u8; 32]| -> Result<Material> {
                if x25519 {
                    Ok(Material::XPublic(bytes))
                } else {
                    ed25519_dalek::VerifyingKey::from_bytes(&bytes)
                        .map_err(|_| data_error("the public key is not valid"))?;
                    Ok(Material::EdPublic(bytes))
                }
            };
            let private_material = |bytes: [u8; 32]| -> Material {
                if x25519 {
                    Material::XPrivate(bytes)
                } else {
                    Material::EdPrivate(bytes)
                }
            };
            match format {
                "raw" => {
                    let material = public_material(key32(raw()?)?)?;
                    let usages = check_usages(&usages, public_allowed)?;
                    Ok(Value::Object(make_key(
                        cx,
                        "public",
                        extractable,
                        algorithm,
                        usages,
                        material,
                    )))
                }
                "spki" => {
                    let material = public_material(okp_from_der(x25519, &raw()?, false)?)?;
                    let usages = check_usages(&usages, public_allowed)?;
                    Ok(Value::Object(make_key(
                        cx,
                        "public",
                        extractable,
                        algorithm,
                        usages,
                        material,
                    )))
                }
                "pkcs8" => {
                    let material = private_material(okp_from_der(x25519, &raw()?, true)?);
                    let usages = check_usages(&usages, private_allowed)?;
                    Ok(Value::Object(make_key(
                        cx,
                        "private",
                        extractable,
                        algorithm,
                        usages,
                        material,
                    )))
                }
                _ => {
                    jwk_matches(cx, "OKP")?;
                    if jwk.string(cx, "crv").as_deref() != Some(name.as_str()) {
                        return Err(data_error("the JWK curve does not match"));
                    }
                    let x = key32(jwk.required(cx, "x")?)?;
                    match jwk.bytes(cx, "d")? {
                        Some(d) => {
                            let d = key32(d)?;
                            let usages = check_usages(&usages, private_allowed)?;
                            Ok(Value::Object(make_key(
                                cx,
                                "private",
                                extractable,
                                algorithm,
                                usages,
                                private_material(d),
                            )))
                        }
                        None => {
                            let material = public_material(x)?;
                            let usages = check_usages(&usages, public_allowed)?;
                            Ok(Value::Object(make_key(
                                cx,
                                "public",
                                extractable,
                                algorithm,
                                usages,
                                material,
                            )))
                        }
                    }
                }
            }
        }
        other => Err(not_supported(format!(
            "the algorithm `{other}` is not supported"
        ))),
    }
}

fn jwk_common(key: &KeySnapshot, mut fields: Vec<(&'static str, Value)>) -> Value {
    fields.push(("ext", Value::Bool(key.extractable)));
    fields.push((
        "key_ops",
        Value::Array(
            key.usages
                .iter()
                .map(|u| Value::String(u.clone()))
                .collect(),
        ),
    ));
    jwk_record(fields)
}

fn export_key(format: &str, key: &KeySnapshot) -> Result<Value> {
    if !key.extractable {
        return Err(invalid_access("the key is not extractable"));
    }
    let name = key.algorithm.name.as_str();
    let unsupported = || {
        Err(not_supported(format!(
            "the `{format}` format is not supported for {name} keys"
        )))
    };
    match &key.material {
        Material::Secret(bytes) => match format {
            "raw" => Ok(Value::ArrayBuffer(bytes.clone())),
            "jwk" => {
                let alg = match (name, key.algorithm.hash) {
                    ("HMAC", Some(hash)) => Some(format!("HS{}", hash.jwk_suffix())),
                    (aes, _) if is_aes(aes) => {
                        key.algorithm.length.map(|l| format!("A{l}{}", &aes[4..]))
                    }
                    _ => None,
                };
                let mut fields = vec![("kty", Value::String("oct".to_string())), ("k", b64(bytes))];
                if let Some(alg) = alg {
                    fields.push(("alg", Value::String(alg)));
                }
                Ok(jwk_common(key, fields))
            }
            _ => unsupported(),
        },
        Material::EcPublic { curve, point } => match format {
            "raw" => Ok(Value::ArrayBuffer(point.clone())),
            "spki" => Ok(Value::ArrayBuffer(ec::public_spki(*curve, point)?)),
            "jwk" => {
                let (x, y) = ec::coordinates(*curve, point);
                Ok(jwk_common(
                    key,
                    vec![
                        ("kty", Value::String("EC".to_string())),
                        ("crv", Value::String(curve.name().to_string())),
                        ("x", b64(&x)),
                        ("y", b64(&y)),
                    ],
                ))
            }
            _ => unsupported(),
        },
        Material::EcPrivate { curve, scalar } => match format {
            "pkcs8" => Ok(Value::ArrayBuffer(ec::private_pkcs8(*curve, scalar)?)),
            "jwk" => {
                let point = ec::public_of(*curve, scalar)?;
                let (x, y) = ec::coordinates(*curve, &point);
                Ok(jwk_common(
                    key,
                    vec![
                        ("kty", Value::String("EC".to_string())),
                        ("crv", Value::String(curve.name().to_string())),
                        ("x", b64(&x)),
                        ("y", b64(&y)),
                        ("d", b64(scalar)),
                    ],
                ))
            }
            _ => unsupported(),
        },
        Material::RsaPublic(public) => match format {
            "spki" => Ok(Value::ArrayBuffer(rsa_ops::public_spki(public)?)),
            "jwk" => {
                let (n, e) = rsa_ops::public_jwk(public);
                Ok(jwk_common(
                    key,
                    rsa_jwk_fields(key, vec![("n", b64(&n)), ("e", b64(&e))]),
                ))
            }
            _ => unsupported(),
        },
        Material::RsaPrivate(private) => match format {
            "pkcs8" => Ok(Value::ArrayBuffer(rsa_ops::private_pkcs8(private)?)),
            "jwk" => {
                let parts = rsa_ops::private_jwk(private);
                let names = ["n", "e", "d", "p", "q", "dp", "dq", "qi"];
                let fields: Vec<(&'static str, Value)> = names
                    .iter()
                    .zip(parts.iter())
                    .map(|(k, v)| (*k, b64(v)))
                    .collect();
                Ok(jwk_common(key, rsa_jwk_fields(key, fields)))
            }
            _ => unsupported(),
        },
        Material::EdPublic(bytes) | Material::XPublic(bytes) => {
            let x25519 = matches!(key.material, Material::XPublic(_));
            match format {
                "raw" => Ok(Value::ArrayBuffer(bytes.to_vec())),
                "spki" => Ok(Value::ArrayBuffer(okp_spki(x25519, bytes))),
                "jwk" => Ok(jwk_common(
                    key,
                    vec![
                        ("kty", Value::String("OKP".to_string())),
                        ("crv", Value::String(name.to_string())),
                        ("x", b64(bytes)),
                    ],
                )),
                _ => unsupported(),
            }
        }
        Material::EdPrivate(bytes) | Material::XPrivate(bytes) => {
            let x25519 = matches!(key.material, Material::XPrivate(_));
            let public = if x25519 {
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*bytes)).to_bytes()
            } else {
                ed25519_dalek::SigningKey::from_bytes(bytes)
                    .verifying_key()
                    .to_bytes()
            };
            match format {
                "pkcs8" => Ok(Value::ArrayBuffer(okp_pkcs8(x25519, bytes))),
                "jwk" => Ok(jwk_common(
                    key,
                    vec![
                        ("kty", Value::String("OKP".to_string())),
                        ("crv", Value::String(name.to_string())),
                        ("x", b64(&public)),
                        ("d", b64(bytes)),
                    ],
                )),
                _ => unsupported(),
            }
        }
    }
}

fn rsa_jwk_fields(
    key: &KeySnapshot,
    mut fields: Vec<(&'static str, Value)>,
) -> Vec<(&'static str, Value)> {
    let alg = match (key.algorithm.name.as_str(), key.algorithm.hash) {
        ("RSASSA-PKCS1-v1_5", Some(h)) => Some(format!("RS{}", h.jwk_suffix())),
        ("RSA-PSS", Some(h)) => Some(format!("PS{}", h.jwk_suffix())),
        ("RSA-OAEP", Some(Hash::Sha1)) => Some("RSA-OAEP".to_string()),
        ("RSA-OAEP", Some(h)) => Some(format!("RSA-OAEP-{}", h.jwk_suffix())),
        _ => None,
    };
    let mut out = vec![("kty", Value::String("RSA".to_string()))];
    if let Some(alg) = alg {
        out.push(("alg", Value::String(alg)));
    }
    out.append(&mut fields);
    out
}

fn encrypt_or_decrypt(
    cx: &mut Cx<'_>,
    params: Params,
    key: KeySnapshot,
    data: Vec<u8>,
    encrypt: bool,
) -> Result<Value> {
    let name = canonical(&params.name);
    key.require_algorithm(&name)?;
    key.require_usage(if encrypt { "encrypt" } else { "decrypt" })?;
    let out = match (name.as_str(), &key.material) {
        ("AES-GCM", Material::Secret(k)) => {
            let iv = params.required_bytes(cx, "iv")?;
            let aad = params.bytes(cx, "additionalData").unwrap_or_default();
            if let Some(tag) = params.number(cx, "tagLength")
                && tag != 128.0
            {
                return Err(not_supported("only 128-bit AES-GCM tags are supported"));
            }
            aes_gcm(k, &iv, &aad, &data, encrypt)?
        }
        ("AES-CBC", Material::Secret(k)) => {
            let iv = params.required_bytes(cx, "iv")?;
            aes_cbc(k, &iv, &data, encrypt)?
        }
        ("AES-CTR", Material::Secret(k)) => {
            let counter = params.required_bytes(cx, "counter")?;
            aes_ctr(k, &counter, &data)?
        }
        ("RSA-OAEP", Material::RsaPublic(public)) if encrypt => {
            let hash = key
                .algorithm
                .hash
                .ok_or_else(|| operation("the key has no hash"))?;
            rsa_ops::oaep_encrypt(public, hash, params.bytes(cx, "label"), &data)?
        }
        ("RSA-OAEP", Material::RsaPrivate(private)) if !encrypt => {
            let hash = key
                .algorithm
                .hash
                .ok_or_else(|| operation("the key has no hash"))?;
            rsa_ops::oaep_decrypt(private, hash, params.bytes(cx, "label"), &data)?
        }
        ("RSA-OAEP", _) => return Err(invalid_access("the key kind does not fit the operation")),
        (other, _) => return Err(not_supported(format!("`{other}` does not encrypt"))),
    };
    Ok(Value::ArrayBuffer(out))
}

fn sign_or_verify(
    cx: &mut Cx<'_>,
    params: Params,
    key: KeySnapshot,
    data: Vec<u8>,
    signature: Option<Vec<u8>>,
) -> Result<Value> {
    let name = canonical(&params.name);
    key.require_algorithm(&name)?;
    let verifying = signature.is_some();
    key.require_usage(if verifying { "verify" } else { "sign" })?;
    let produced: Option<Vec<u8>>;
    let verified: Option<bool>;
    match (name.as_str(), &key.material) {
        ("HMAC", Material::Secret(k)) => {
            let hash = key
                .algorithm
                .hash
                .ok_or_else(|| operation("the key has no hash"))?;
            let mac = hash.hmac(k, &data);
            match &signature {
                Some(sig) => {
                    verified = Some(sig.len() == mac.len() && constant_time_eq(sig, &mac));
                    produced = None;
                }
                None => {
                    produced = Some(mac);
                    verified = None;
                }
            }
        }
        ("ECDSA", Material::EcPrivate { curve, scalar }) if !verifying => {
            let hash = params.required_hash(cx)?;
            produced = Some(ec::sign(*curve, scalar, &hash.digest(&data))?);
            verified = None;
        }
        ("ECDSA", Material::EcPublic { curve, point }) if verifying => {
            let hash = params.required_hash(cx)?;
            let sig = signature.unwrap_or_default();
            verified = Some(ec::verify(*curve, point, &hash.digest(&data), &sig));
            produced = None;
        }
        ("RSASSA-PKCS1-v1_5", Material::RsaPrivate(private)) if !verifying => {
            let hash = key
                .algorithm
                .hash
                .ok_or_else(|| operation("the key has no hash"))?;
            produced = Some(rsa_ops::pkcs1v15_sign(private, hash, &hash.digest(&data))?);
            verified = None;
        }
        ("RSASSA-PKCS1-v1_5", Material::RsaPublic(public)) if verifying => {
            let hash = key
                .algorithm
                .hash
                .ok_or_else(|| operation("the key has no hash"))?;
            verified = Some(rsa_ops::pkcs1v15_verify(
                public,
                hash,
                &hash.digest(&data),
                &signature.unwrap_or_default(),
            ));
            produced = None;
        }
        ("RSA-PSS", Material::RsaPrivate(private)) if !verifying => {
            let hash = key
                .algorithm
                .hash
                .ok_or_else(|| operation("the key has no hash"))?;
            let salt = params.number(cx, "saltLength").unwrap_or(0.0) as usize;
            produced = Some(rsa_ops::pss_sign(private, hash, salt, &hash.digest(&data))?);
            verified = None;
        }
        ("RSA-PSS", Material::RsaPublic(public)) if verifying => {
            let hash = key
                .algorithm
                .hash
                .ok_or_else(|| operation("the key has no hash"))?;
            let salt = params.number(cx, "saltLength").unwrap_or(0.0) as usize;
            verified = Some(rsa_ops::pss_verify(
                public,
                hash,
                salt,
                &hash.digest(&data),
                &signature.unwrap_or_default(),
            ));
            produced = None;
        }
        ("Ed25519", Material::EdPrivate(secret)) if !verifying => {
            use ed25519_dalek::Signer as _;
            let key = ed25519_dalek::SigningKey::from_bytes(secret);
            produced = Some(key.sign(&data).to_bytes().to_vec());
            verified = None;
        }
        ("Ed25519", Material::EdPublic(public)) if verifying => {
            let sig = signature.unwrap_or_default();
            verified = Some(
                match (
                    ed25519_dalek::VerifyingKey::from_bytes(public),
                    ed25519_dalek::Signature::from_slice(&sig),
                ) {
                    (Ok(key), Ok(sig)) => key.verify_strict(&data, &sig).is_ok(),
                    _ => false,
                },
            );
            produced = None;
        }
        ("HMAC" | "ECDSA" | "RSASSA-PKCS1-v1_5" | "RSA-PSS" | "Ed25519", _) => {
            return Err(invalid_access("the key kind does not fit the operation"));
        }
        (other, _) => return Err(not_supported(format!("`{other}` does not sign"))),
    }
    Ok(match (produced, verified) {
        (Some(bytes), _) => Value::ArrayBuffer(bytes),
        (None, Some(ok)) => Value::Bool(ok),
        _ => Value::Undefined,
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `deriveBits`; `length` in bits, `None` for all the algorithm gives.
fn derive_bits(
    cx: &mut Cx<'_>,
    params: Params,
    key: KeySnapshot,
    length: Option<u32>,
) -> Result<Vec<u8>> {
    let name = canonical(&params.name);
    key.require_algorithm(&name)?;
    key.require_usage("deriveBits")
        .or_else(|_| key.require_usage("deriveKey"))?;
    match (name.as_str(), &key.material) {
        ("PBKDF2", Material::Secret(password)) => {
            let length = length.ok_or_else(|| operation("PBKDF2 needs a length"))?;
            if length == 0 || !length.is_multiple_of(8) {
                return Err(operation("the length must be a positive multiple of 8"));
            }
            let hash = params.required_hash(cx)?;
            let salt = params.required_bytes(cx, "salt")?;
            let iterations = params
                .number(cx, "iterations")
                .map(|n| n as u32)
                .filter(|n| *n > 0)
                .ok_or_else(|| operation("`iterations` must be a positive number"))?;
            let mut out = vec![0u8; (length / 8) as usize];
            hash.pbkdf2(password, &salt, iterations, &mut out);
            Ok(out)
        }
        ("HKDF", Material::Secret(ikm)) => {
            let length = length.ok_or_else(|| operation("HKDF needs a length"))?;
            if !length.is_multiple_of(8) {
                return Err(operation("the length must be a multiple of 8"));
            }
            let hash = params.required_hash(cx)?;
            let salt = params.required_bytes(cx, "salt")?;
            let info = params.required_bytes(cx, "info")?;
            let mut out = vec![0u8; (length / 8) as usize];
            hash.hkdf(ikm, &salt, &info, &mut out)?;
            Ok(out)
        }
        ("ECDH", Material::EcPrivate { curve, scalar }) => {
            let public = params.key(cx, "public")?;
            let Material::EcPublic {
                curve: other,
                point,
            } = &public.material
            else {
                return Err(invalid_access("`public` must be an ECDH public key"));
            };
            if other != curve || !public.algorithm.name.eq_ignore_ascii_case("ECDH") {
                return Err(invalid_access("the public key is not on the same curve"));
            }
            let secret = ec::agree(*curve, scalar, point)?;
            truncate_bits(secret, length)
        }
        ("X25519", Material::XPrivate(secret)) => {
            let public = params.key(cx, "public")?;
            let Material::XPublic(point) = &public.material else {
                return Err(invalid_access("`public` must be an X25519 public key"));
            };
            let shared = x25519_dalek::StaticSecret::from(*secret)
                .diffie_hellman(&x25519_dalek::PublicKey::from(*point));
            if shared.as_bytes().iter().all(|b| *b == 0) {
                return Err(operation("the shared secret is all zeros"));
            }
            truncate_bits(shared.to_bytes().to_vec(), length)
        }
        ("PBKDF2" | "HKDF" | "ECDH" | "X25519", _) => {
            Err(invalid_access("the key kind does not fit the operation"))
        }
        (other, _) => Err(not_supported(format!("`{other}` does not derive bits"))),
    }
}

fn truncate_bits(mut secret: Vec<u8>, length: Option<u32>) -> Result<Vec<u8>> {
    match length {
        None | Some(0) => Ok(secret),
        Some(bits) => {
            let bytes = bits.div_ceil(8) as usize;
            if bytes > secret.len() {
                return Err(operation(
                    "the requested length is longer than the shared secret",
                ));
            }
            secret.truncate(bytes);
            if !bits.is_multiple_of(8)
                && let Some(last) = secret.last_mut()
            {
                *last &= 0xffu8 << (8 - bits % 8);
            }
            Ok(secret)
        }
    }
}

/// The key length in bits a derived key of `params` type needs.
fn derived_length(cx: &mut Cx<'_>, params: &Params) -> Result<u32> {
    let name = canonical(&params.name);
    match name.as_str() {
        "HMAC" => {
            let hash = params.required_hash(cx)?;
            Ok(params
                .number(cx, "length")
                .map(|n| n as u32)
                .unwrap_or(hash.block_bits()))
        }
        aes if is_aes(aes) => params
            .number(cx, "length")
            .map(|n| n as u32)
            .filter(|l| matches!(l, 128 | 192 | 256))
            .ok_or_else(|| operation("an AES key length must be 128, 192 or 256")),
        "HKDF" | "PBKDF2" => Err(not_supported(
            "derived keys of that type take any length; use deriveBits",
        )),
        other => Err(not_supported(format!("`{other}` keys cannot be derived"))),
    }
}

impl web::SubtleCryptoImpl for Web {
    fn digest(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        data: Vec<u8>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = algorithm_name(cx, &algorithm)
            .and_then(|name| Hash::parse(&name))
            .map(|hash| Value::ArrayBuffer(hash.digest(&data)));
        Ok(settle(cx, result))
    }

    fn encrypt(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        key: ObjectId,
        data: Vec<u8>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            let params = Params::read(cx, &algorithm)?;
            let key = key_of(cx, key)?;
            encrypt_or_decrypt(cx, params, key, data, true)
        })();
        Ok(settle(cx, result))
    }

    fn decrypt(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        key: ObjectId,
        data: Vec<u8>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            let params = Params::read(cx, &algorithm)?;
            let key = key_of(cx, key)?;
            encrypt_or_decrypt(cx, params, key, data, false)
        })();
        Ok(settle(cx, result))
    }

    fn sign(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        key: ObjectId,
        data: Vec<u8>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            let params = Params::read(cx, &algorithm)?;
            let key = key_of(cx, key)?;
            sign_or_verify(cx, params, key, data, None)
        })();
        Ok(settle(cx, result))
    }

    fn verify(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        key: ObjectId,
        signature: Vec<u8>,
        data: Vec<u8>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            let params = Params::read(cx, &algorithm)?;
            let key = key_of(cx, key)?;
            sign_or_verify(cx, params, key, data, Some(signature))
        })();
        Ok(settle(cx, result))
    }

    fn generate_key(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        extractable: bool,
        key_usages: Vec<String>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            let params = Params::read(cx, &algorithm)?;
            generate_key(cx, params, extractable, key_usages)
        })();
        Ok(settle(cx, result))
    }

    fn derive_key(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        base_key: ObjectId,
        derived_key_type: Value,
        extractable: bool,
        key_usages: Vec<String>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            let params = Params::read(cx, &algorithm)?;
            let key = key_of(cx, base_key)?;
            key.require_usage("deriveKey")?;
            let derived = Params::read(cx, &derived_key_type)?;
            let length = derived_length(cx, &derived)?;
            let bits = derive_bits(cx, params, key, Some(length))?;
            import_key(
                cx,
                "raw",
                Value::ArrayBuffer(bits),
                derived,
                extractable,
                key_usages,
            )
        })();
        Ok(settle(cx, result))
    }

    fn derive_bits(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        base_key: ObjectId,
        length: Option<u32>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            let params = Params::read(cx, &algorithm)?;
            let key = key_of(cx, base_key)?;
            derive_bits(cx, params, key, length).map(Value::ArrayBuffer)
        })();
        Ok(settle(cx, result))
    }

    fn import_key(
        cx: &mut Cx<'_>,
        this: ObjectId,
        format: String,
        key_data: Value,
        algorithm: Value,
        extractable: bool,
        key_usages: Vec<String>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            if !matches!(format.as_str(), "raw" | "jwk" | "pkcs8" | "spki") {
                return Err(Exception::type_error(format!(
                    "`{format}` is not a key format"
                )));
            }
            let params = Params::read(cx, &algorithm)?;
            import_key(cx, &format, key_data, params, extractable, key_usages)
        })();
        Ok(settle(cx, result))
    }

    fn export_key(
        cx: &mut Cx<'_>,
        this: ObjectId,
        format: String,
        key: ObjectId,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let result = (|| {
            if !matches!(format.as_str(), "raw" | "jwk" | "pkcs8" | "spki") {
                return Err(Exception::type_error(format!(
                    "`{format}` is not a key format"
                )));
            }
            let key = key_of(cx, key)?;
            export_key(&format, &key)
        })();
        Ok(settle(cx, result))
    }
}
