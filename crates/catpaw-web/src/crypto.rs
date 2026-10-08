//! `window.crypto`: random values and digests
//! (<https://w3c.github.io/webcrypto/>).
//!
//! `crypto.subtle` lives in `webcrypto`.
//!
//! Randomness is drawn for the realm script runs in ([`enter_realm`]):
//! from the operating system, or, in a seeded run, from a repeatable
//! sequence of the realm's own ([`seed_realm`]), so that documents, frames
//! and workers never share or restart one another's sequences.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use catpaw_js::{Exception, Fallible, ObjectId, Value};

use crate::generated as web;
use crate::page::{Cx, PageState};
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
    /// The seeded sequences of the realms on this thread, by the epoch of
    /// their page.
    static SEQUENCES: RefCell<HashMap<u64, u64>> = RefCell::new(HashMap::new());
    /// The epoch of the page whose realm script runs in (0: none).
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

/// Gives the realm of `page` a repeatable random sequence starting from
/// `seed`; `None` leaves it the operating system's randomness.
pub fn seed_realm(page: &PageState, seed: Option<u64>) {
    SEQUENCES.with(|sequences| {
        let mut sequences = sequences.borrow_mut();
        match seed {
            Some(seed) => sequences.insert(page.epoch, seed),
            None => sequences.remove(&page.epoch),
        };
    });
}

/// Forgets the sequence of a realm that is gone.
pub fn forget_realm(page: &PageState) {
    SEQUENCES.with(|sequences| sequences.borrow_mut().remove(&page.epoch));
}

/// The realm randomness is drawn for; dropping it returns to the one
/// before.
#[must_use = "the realm is left when this is dropped"]
pub struct EnteredRealm {
    previous: u64,
}

impl Drop for EnteredRealm {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.previous));
    }
}

/// Makes the realm of `page` the one randomness is drawn for. Script
/// bindings call this whenever they run code for a page.
pub fn enter_realm(page: &PageState) -> EnteredRealm {
    EnteredRealm {
        previous: CURRENT.with(|current| current.replace(page.epoch)),
    }
}

/// SplitMix64.
fn next_seeded(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fills `bytes` with randomness for the realm script runs in: its seeded
/// sequence in a seeded run, else the operating system's.
pub fn fill_random(bytes: &mut [u8]) -> Fallible<()> {
    let current = CURRENT.with(Cell::get);
    let seeded = SEQUENCES.with(|sequences| {
        let mut sequences = sequences.borrow_mut();
        let state = sequences.get_mut(&current)?;
        for chunk in bytes.chunks_mut(8) {
            let word = next_seeded(state).to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Some(())
    });
    if seeded.is_some() {
        return Ok(());
    }
    getrandom::fill(bytes)
        .map_err(|_| Exception::dom("OperationError", "No source of randomness is available"))
}

/// The generator Web Crypto makes keys, salts and padding with: the
/// realm's randomness ([`fill_random`]). In a seeded run what it makes is
/// as predictable as the seed, which is the point of one.
pub(crate) struct RealmRng;

impl rand::RngCore for RealmRng {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        fill_random(dest).expect("no source of randomness is available");
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        fill_random(dest).map_err(|_| rand::Error::new("no source of randomness is available"))
    }
}

impl rand::CryptoRng for RealmRng {}

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
