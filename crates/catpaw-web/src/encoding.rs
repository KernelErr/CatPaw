//! `TextEncoder` and `TextDecoder` (<https://encoding.spec.whatwg.org/#api>).

use catpaw_js::{Exception, Fallible, ObjectId, Uint8ArrayData};
use encoding_rs::{Decoder, DecoderResult, Encoding};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, platform_object};

pub struct TextEncoderObject;
platform_object!(TextEncoderObject, TextEncoder);

pub struct TextDecoderObject {
    encoding: &'static Encoding,
    fatal: bool,
    ignore_bom: bool,
    /// The decoder of a stream in progress (`decode(..., {stream: true})`).
    decoder: Option<Decoder>,
}
platform_object!(TextDecoderObject, TextDecoder);

impl TextDecoderObject {
    fn new_decoder(&self) -> Decoder {
        if self.ignore_bom {
            self.encoding.new_decoder_without_bom_handling()
        } else {
            self.encoding.new_decoder_with_bom_removal()
        }
    }

    fn decode(&mut self, input: &[u8], stream: bool) -> Result<String, ()> {
        let mut decoder = self.decoder.take().unwrap_or_else(|| self.new_decoder());
        let last = !stream;
        let mut out = String::new();
        if self.fatal {
            let capacity = decoder
                .max_utf8_buffer_length_without_replacement(input.len())
                .ok_or(())?;
            out.reserve(capacity);
            let (result, _) = decoder.decode_to_string_without_replacement(input, &mut out, last);
            if !matches!(result, DecoderResult::InputEmpty) {
                return Err(());
            }
        } else {
            let capacity = decoder.max_utf8_buffer_length(input.len()).ok_or(())?;
            out.reserve(capacity);
            let _ = decoder.decode_to_string(input, &mut out, last);
        }
        if stream {
            self.decoder = Some(decoder);
        }
        Ok(out)
    }
}

impl web::TextEncoderImpl for Web {
    fn encode(_cx: &mut Cx<'_>, _this: ObjectId, input: String) -> Fallible<Uint8ArrayData> {
        Ok(Uint8ArrayData(input.into_bytes()))
    }

    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(TextEncoderObject))
    }
}

impl web::TextEncoderCommonImpl for Web {
    fn encoding(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("utf-8".to_string())
    }
}

impl web::TextDecoderImpl for Web {
    fn decode(
        cx: &mut Cx<'_>,
        this: ObjectId,
        input: Option<Vec<u8>>,
        options: web::TextDecodeOptions,
    ) -> Fallible<String> {
        let input = input.unwrap_or_default();
        cx.page
            .with::<TextDecoderObject, _>(this, |d| d.decode(&input, options.stream))?
            .map_err(|()| Exception::type_error("The encoded data was not valid"))
    }

    fn constructor(
        cx: &mut Cx<'_>,
        label: String,
        options: web::TextDecoderOptions,
    ) -> Fallible<ObjectId> {
        let encoding = Encoding::for_label(label.trim().as_bytes())
            .filter(|e| *e != encoding_rs::REPLACEMENT)
            .ok_or_else(|| {
                Exception::range_error(format!("The encoding label '{label}' is not supported"))
            })?;
        Ok(cx.page.alloc(TextDecoderObject {
            encoding,
            fatal: options.fatal,
            ignore_bom: options.ignore_bom,
            decoder: None,
        }))
    }
}

impl web::TextDecoderCommonImpl for Web {
    fn encoding(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page
            .with::<TextDecoderObject, _>(this, |d| d.encoding.name().to_ascii_lowercase())
    }

    fn fatal(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        cx.page.with::<TextDecoderObject, _>(this, |d| d.fatal)
    }

    fn ignore_bom(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        cx.page.with::<TextDecoderObject, _>(this, |d| d.ignore_bom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(label: &str, fatal: bool) -> TextDecoderObject {
        TextDecoderObject {
            encoding: Encoding::for_label(label.as_bytes()).unwrap(),
            fatal,
            ignore_bom: false,
            decoder: None,
        }
    }

    #[test]
    fn decodes_streams_across_chunk_boundaries() {
        let mut d = decoder("utf-8", false);
        let bytes = "héllo".as_bytes();
        let mut out = d.decode(&bytes[..2], true).unwrap();
        out.push_str(&d.decode(&bytes[2..], false).unwrap());
        assert_eq!(out, "héllo");
        // A finished stream starts afresh, BOM handling included.
        assert_eq!(d.decode(b"\xEF\xBB\xBFx", false).unwrap(), "x");
    }

    #[test]
    fn fatal_mode_rejects_malformed_input() {
        assert!(decoder("utf-8", true).decode(b"\xFF", false).is_err());
        assert_eq!(
            decoder("utf-8", false).decode(b"\xFF", false).unwrap(),
            "\u{FFFD}"
        );
        assert_eq!(
            decoder("gbk", true).decode(b"\xD6\xD0", false).unwrap(),
            "中"
        );
    }
}
