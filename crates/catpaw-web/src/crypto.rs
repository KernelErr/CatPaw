//! `window.crypto`: random values and digests
//! (<https://w3c.github.io/webcrypto/>).
//!
//! `crypto.subtle` offers `digest()` with the SHA family; the key-based
//! operations are absent rather than present and failing, so pages can
//! tell.

use catpaw_js::{Exception, Fallible, ObjectId, PromiseRef, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, platform_object};

pub struct CryptoObject;
platform_object!(CryptoObject, Crypto);

pub struct SubtleCryptoObject;
platform_object!(SubtleCryptoObject, SubtleCrypto);

/// The most `getRandomValues()` hands out in one call.
pub const RANDOM_VALUES_LIMIT: usize = 65536;

/// Fills `bytes` from the operating system's source of randomness.
pub fn fill_random(bytes: &mut [u8]) -> Fallible<()> {
    getrandom::fill(bytes)
        .map_err(|_| Exception::dom("OperationError", "No source of randomness is available"))
}

impl web::CryptoImpl for Web {
    fn subtle(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        cx.page.with::<CryptoObject, _>(this, |_| ())?;
        Ok(crate::window::singleton(
            cx,
            |s| &mut s.subtle,
            |page| page.alloc(SubtleCryptoObject),
        ))
    }

    fn random_uuid(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page.with::<CryptoObject, _>(this, |_| ())?;
        let mut bytes = [0u8; 16];
        fill_random(&mut bytes)?;
        // Version 4, variant 1.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!(
            "{}-{}-{}-{}-{}",
            hex[0..4].concat(),
            hex[4..6].concat(),
            hex[6..8].concat(),
            hex[8..10].concat(),
            hex[10..16].concat()
        ))
    }
}

/// The algorithm name of an `AlgorithmIdentifier`: a string, or an
/// object's `name`.
fn algorithm_name(cx: &mut Cx<'_>, algorithm: &Value) -> Fallible<String> {
    let name = match algorithm {
        Value::String(s) => s.clone(),
        Value::Object(_) | Value::Opaque(_) => match cx.script.get_property(algorithm, "name")? {
            Value::String(s) => s,
            _ => {
                return Err(Exception::type_error(
                    "Failed to normalize the algorithm: a name is required",
                ));
            }
        },
        _ => {
            return Err(Exception::type_error(
                "Failed to normalize the algorithm: it is not an object or a string",
            ));
        }
    };
    Ok(name)
}

impl web::SubtleCryptoImpl for Web {
    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-digest>
    fn digest(
        cx: &mut Cx<'_>,
        this: ObjectId,
        algorithm: Value,
        data: Vec<u8>,
    ) -> Fallible<PromiseRef> {
        cx.page.with::<SubtleCryptoObject, _>(this, |_| ())?;
        let promise = cx.script.new_promise();
        // A bad algorithm rejects rather than throws.
        let name = match algorithm_name(cx, &algorithm) {
            Ok(name) => name,
            Err(e) => {
                cx.script.reject_promise(&promise, e);
                return Ok(promise);
            }
        };
        let hashed: Option<Vec<u8>> = match name.to_ascii_uppercase().as_str() {
            "SHA-1" => Some(Sha1::digest(&data).to_vec()),
            "SHA-256" => Some(Sha256::digest(&data).to_vec()),
            "SHA-384" => Some(Sha384::digest(&data).to_vec()),
            "SHA-512" => Some(Sha512::digest(&data).to_vec()),
            _ => None,
        };
        match hashed {
            Some(bytes) => cx
                .script
                .resolve_promise(&promise, Value::ArrayBuffer(bytes)),
            None => cx.script.reject_promise(
                &promise,
                Exception::not_supported(format!("The algorithm `{name}` is not supported")),
            ),
        }
        Ok(promise)
    }
}
