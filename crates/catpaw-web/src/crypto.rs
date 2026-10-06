//! `window.crypto`: random values (<https://w3c.github.io/webcrypto/>).
//!
//! `SubtleCrypto` is not there yet, and `crypto.subtle` is absent rather
//! than present and failing.

use catpaw_js::{Exception, Fallible, ObjectId};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, platform_object};

pub struct CryptoObject;
platform_object!(CryptoObject, Crypto);

/// The most `getRandomValues()` hands out in one call.
pub const RANDOM_VALUES_LIMIT: usize = 65536;

/// Fills `bytes` from the operating system's source of randomness.
pub fn fill_random(bytes: &mut [u8]) -> Fallible<()> {
    getrandom::fill(bytes)
        .map_err(|_| Exception::dom("OperationError", "No source of randomness is available"))
}

impl web::CryptoImpl for Web {
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
