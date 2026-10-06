//! Determining the character encoding of an HTML document.
//!
//! Follows the order of the HTML specification's "determine the character
//! encoding" algorithm: byte order mark, transport-layer charset, the
//! `<meta>` prescan of the first 1024 bytes, then a default. The default
//! deviates from the spec's locale table on purpose: undeclared documents
//! that are valid UTF-8 decode as UTF-8, everything else as windows-1252.

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingSource {
    Bom,
    Transport,
    MetaPrescan,
    Default,
}

#[derive(Debug)]
pub struct DecodedDocument {
    pub text: String,
    /// The WHATWG encoding name actually used (e.g. `UTF-8`, `windows-1252`).
    pub encoding: &'static str,
    pub source: EncodingSource,
    pub had_errors: bool,
}

fn usable(encoding: &'static Encoding) -> Option<&'static Encoding> {
    // `replacement` is what dangerous labels (utf-7, hz-gb-2312, ...) map to;
    // treating it as undeclared is safer than emitting U+FFFD soup.
    if encoding == encoding_rs::REPLACEMENT {
        None
    } else {
        Some(encoding)
    }
}

/// Decodes an HTML document's bytes.
pub fn decode_document(bytes: &[u8], transport_charset: Option<&str>) -> DecodedDocument {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        let (text, had_errors) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return DecodedDocument {
            text: text.into_owned(),
            encoding: encoding.name(),
            source: EncodingSource::Bom,
            had_errors,
        };
    }
    if let Some(encoding) = transport_charset
        .and_then(|label| Encoding::for_label(label.trim().as_bytes()))
        .and_then(usable)
    {
        let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
        return DecodedDocument {
            text: text.into_owned(),
            encoding: encoding.name(),
            source: EncodingSource::Transport,
            had_errors,
        };
    }
    if let Some(encoding) = prescan_meta_charset(bytes).and_then(usable) {
        let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
        return DecodedDocument {
            text: text.into_owned(),
            encoding: encoding.name(),
            source: EncodingSource::MetaPrescan,
            had_errors,
        };
    }
    let encoding = if std::str::from_utf8(bytes).is_ok() {
        UTF_8
    } else {
        WINDOWS_1252
    };
    let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
    DecodedDocument {
        text: text.into_owned(),
        encoding: encoding.name(),
        source: EncodingSource::Default,
        had_errors,
    }
}

/// A simplified "prescan a byte stream to determine its encoding": looks at
/// the first 1024 bytes for `<meta charset=...>` or a `content="...;
/// charset=..."` attribute and returns the labelled encoding. UTF-16 labels
/// are mapped to UTF-8 as the spec requires.
pub fn prescan_meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let window = &bytes[..bytes.len().min(1024)];
    let lower: Vec<u8> = window.iter().map(u8::to_ascii_lowercase).collect();
    let mut pos = 0;
    while let Some(start) = find(&lower[pos..], b"<meta") {
        let tag_start = pos + start;
        let tag_end = find(&lower[tag_start..], b">")
            .map(|e| tag_start + e)
            .unwrap_or(lower.len());
        let tag = &lower[tag_start..tag_end];
        if let Some(label) = charset_in_tag(tag) {
            let text = String::from_utf8_lossy(label);
            let encoding = Encoding::for_label(text.trim().as_bytes())?;
            return Some(
                if encoding == encoding_rs::UTF_16BE || encoding == encoding_rs::UTF_16LE {
                    UTF_8
                } else {
                    encoding
                },
            );
        }
        pos = tag_end;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Extracts the value following `charset=` inside a lowercased `<meta ...` tag.
fn charset_in_tag(tag: &[u8]) -> Option<&[u8]> {
    let idx = find(tag, b"charset")?;
    let mut rest = &tag[idx + b"charset".len()..];
    rest = trim_start(rest);
    rest = rest.strip_prefix(b"=")?;
    rest = trim_start(rest);
    let (quote, body) = match rest.first() {
        Some(q @ (b'"' | b'\'')) => (Some(*q), &rest[1..]),
        _ => (None, rest),
    };
    let end = body
        .iter()
        .position(|&b| match quote {
            Some(q) => b == q,
            None => matches!(b, b';' | b'>' | b'\'' | b'"') || b.is_ascii_whitespace(),
        })
        .unwrap_or(body.len());
    let value = &body[..end];
    if value.is_empty() { None } else { Some(value) }
}

fn trim_start(mut s: &[u8]) -> &[u8] {
    while let Some((first, rest)) = s.split_first()
        && first.is_ascii_whitespace()
    {
        s = rest;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_wins() {
        let d = decode_document(b"\xEF\xBB\xBFhi", Some("windows-1252"));
        assert_eq!(d.text, "hi");
        assert_eq!(d.source, EncodingSource::Bom);
        assert_eq!(d.encoding, "UTF-8");
    }

    #[test]
    fn transport_charset_is_used() {
        let d = decode_document(b"caf\xE9", Some("ISO-8859-1"));
        assert_eq!(d.text, "café");
        assert_eq!(d.source, EncodingSource::Transport);
        assert_eq!(d.encoding, "windows-1252");
    }

    #[test]
    fn meta_prescan_finds_charset_variants() {
        let html = b"<html><head><meta charset=\"shift_jis\"></head>";
        assert_eq!(prescan_meta_charset(html).unwrap().name(), "Shift_JIS");
        let html = b"<META HTTP-EQUIV='Content-Type' CONTENT='text/html; charset=gb2312'>";
        assert_eq!(prescan_meta_charset(html).unwrap().name(), "GBK");
        let html = b"<meta name=viewport content=width=device-width><meta charset=utf-16le>";
        assert_eq!(prescan_meta_charset(html).unwrap().name(), "UTF-8");
        assert!(prescan_meta_charset(b"<meta name=x>").is_none());
    }

    #[test]
    fn default_prefers_utf8_when_valid() {
        let d = decode_document("日本語".as_bytes(), None);
        assert_eq!(d.text, "日本語");
        assert_eq!(d.source, EncodingSource::Default);
        assert_eq!(d.encoding, "UTF-8");
        let d = decode_document(b"na\xEFve", None);
        assert_eq!(d.text, "naïve");
        assert_eq!(d.encoding, "windows-1252");
    }

    #[test]
    fn dangerous_labels_fall_through() {
        let d = decode_document(b"abc", Some("utf-7"));
        assert_eq!(d.source, EncodingSource::Default);
    }
}
