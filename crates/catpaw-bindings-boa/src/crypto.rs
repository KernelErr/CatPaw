//! `crypto.getRandomValues()`, written by hand: it fills the array it is
//! given and returns that same array, which the generated glue, working on
//! copies of buffers, cannot do.

use boa_engine::builtins::typed_array::TypedArrayKind;
use boa_engine::object::builtins::{JsArrayBuffer, JsTypedArray};
use boa_engine::{Context, JsResult, JsValue};
use catpaw_js::Exception;
use catpaw_web::InterfaceId as I;
use catpaw_web::crypto::{RANDOM_VALUES_LIMIT, fill_random};

use crate::rt;

pub(crate) fn get_random_values(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    rt::this_object(this, I::Crypto, ctx)?;
    let Some(array) = args.first() else {
        return Err(rt::type_error("1 argument required, but only 0 present."));
    };
    let mismatch = |ctx: &mut Context| {
        let error = Exception::dom(
            "TypeMismatchError",
            "The argument is not an array of integers",
        );
        rt::exception_to_js(error, ctx)
    };
    let view = array
        .as_object()
        .and_then(|object| JsTypedArray::from_object(object).ok());
    let Some(view) = view else {
        return Err(rt::type_error("The argument is not an ArrayBufferView"));
    };
    let integers = matches!(
        view.kind(),
        Some(
            TypedArrayKind::Int8
                | TypedArrayKind::Uint8
                | TypedArrayKind::Uint8Clamped
                | TypedArrayKind::Int16
                | TypedArrayKind::Uint16
                | TypedArrayKind::Int32
                | TypedArrayKind::Uint32
                | TypedArrayKind::BigInt64
                | TypedArrayKind::BigUint64
        )
    );
    if !integers {
        return Err(mismatch(ctx));
    }
    let offset = view.byte_offset(ctx)?;
    let length = view.byte_length(ctx)?;
    if length > RANDOM_VALUES_LIMIT {
        let error = Exception::quota_exceeded(format!(
            "The array is {length} bytes long, more than the {RANDOM_VALUES_LIMIT} that can be filled at once"
        ));
        return Err(rt::exception_to_js(error, ctx));
    }
    let mut bytes = vec![0u8; length];
    fill_random(&mut bytes).map_err(|e| rt::exception_to_js(e, ctx))?;

    let buffer = view.buffer(ctx)?;
    let buffer = buffer
        .as_object()
        .and_then(|object| JsArrayBuffer::from_object(object).ok())
        .ok_or_else(|| rt::type_error("The array's buffer cannot be written to"))?;
    let mut data = buffer
        .data_mut()
        .ok_or_else(|| rt::type_error("The array's buffer is detached"))?;
    let target = data
        .get_mut(offset..offset + length)
        .ok_or_else(|| rt::type_error("The array is out of bounds"))?;
    target.copy_from_slice(&bytes);
    Ok(array.clone())
}
