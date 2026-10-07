//! `window.crypto`: random values and digests
//! (<https://w3c.github.io/webcrypto/>).
//!
//! `crypto.subtle` lives in `webcrypto`.

use catpaw_js::{Exception, Fallible, ObjectId, Value};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, platform_object};

pub struct CryptoObject;
platform_object!(CryptoObject, Crypto);

pub struct SubtleCryptoObject;
platform_object!(SubtleCryptoObject, SubtleCrypto);

/// A version 4 UUID from the system's randomness.
pub(crate) fn uuid_v4() -> Fallible<String> {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes)?;
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

/// The most `getRandomValues()` hands out in one call.
pub const RANDOM_VALUES_LIMIT: usize = 65536;

thread_local! {
    /// A seeded sequence replacing the system's randomness on this thread.
    static SEEDED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Makes the randomness pages on this thread get a repeatable sequence
/// from `seed` (`None`: the system's again).
pub fn set_random_seed(seed: Option<u64>) {
    SEEDED.with(|s| s.set(seed));
}

/// SplitMix64.
fn next_seeded(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fills `bytes` from the operating system's source of randomness (or the
/// seeded sequence, see [`set_random_seed`]).
pub fn fill_random(bytes: &mut [u8]) -> Fallible<()> {
    let seeded = SEEDED.with(|s| {
        let mut state = s.get()?;
        for chunk in bytes.chunks_mut(8) {
            let word = next_seeded(&mut state).to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        s.set(Some(state));
        Some(())
    });
    if seeded.is_some() {
        return Ok(());
    }
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
pub(crate) fn algorithm_name(cx: &mut Cx<'_>, algorithm: &Value) -> Fallible<String> {
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
