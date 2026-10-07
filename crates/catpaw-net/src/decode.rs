//! `Content-Encoding` decoding for fully buffered bodies.

use std::io::Read;

use bytes::Bytes;
use http::HeaderMap;
use http::header::CONTENT_ENCODING;

/// Why a body could not be decoded.
#[derive(Debug)]
pub enum DecodeError {
    /// Decoding would exceed this many bytes.
    TooLarge(usize),
    Failed(String),
}

impl From<String> for DecodeError {
    fn from(message: String) -> Self {
        DecodeError::Failed(message)
    }
}

/// Decodes `body` according to the response's `Content-Encoding` header.
/// Encodings are listed in the order they were applied, so they are undone
/// in reverse. Output larger than `limit` bytes is refused rather than
/// produced: a small body can decompress to a huge one.
pub fn decode_body(headers: &HeaderMap, body: Bytes, limit: usize) -> Result<Bytes, DecodeError> {
    let Some(encoding) = headers.get(CONTENT_ENCODING).and_then(|v| v.to_str().ok()) else {
        return Ok(body);
    };
    let tokens: Vec<String> = encoding
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty() && t != "identity")
        .collect();
    if tokens.is_empty() {
        return Ok(body);
    }
    let mut data = body.to_vec();
    for token in tokens.iter().rev() {
        data = match token.as_str() {
            "gzip" | "x-gzip" => read_all(flate2::read::MultiGzDecoder::new(&data[..]), limit)?,
            // RFC 9110 says zlib-wrapped, but raw DEFLATE streams exist in the wild.
            "deflate" => match read_all(flate2::read::ZlibDecoder::new(&data[..]), limit) {
                Ok(out) => out,
                Err(DecodeError::TooLarge(limit)) => return Err(DecodeError::TooLarge(limit)),
                Err(_) => read_all(flate2::read::DeflateDecoder::new(&data[..]), limit)?,
            },
            "br" => read_all(brotli::Decompressor::new(&data[..], 8192), limit)?,
            "zstd" => read_all(
                zstd::Decoder::new(&data[..]).map_err(|e| e.to_string())?,
                limit,
            )?,
            other => {
                return Err(DecodeError::Failed(format!(
                    "unsupported content-encoding `{other}`"
                )));
            }
        };
    }
    Ok(Bytes::from(data))
}

fn read_all(reader: impl Read, limit: usize) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    // One byte past the limit tells "too large" from an exact fit.
    reader
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| DecodeError::Failed(format!("decompression failed: {e}")))?;
    if out.len() > limit {
        return Err(DecodeError::TooLarge(limit));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use std::io::Write;

    fn headers(encoding: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_ENCODING, HeaderValue::from_str(encoding).unwrap());
        h
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn passes_identity_through() {
        let body = Bytes::from_static(b"hello");
        assert_eq!(
            decode_body(&HeaderMap::new(), body.clone(), usize::MAX).unwrap(),
            body
        );
        assert_eq!(
            decode_body(&headers("identity"), body.clone(), usize::MAX).unwrap(),
            body
        );
    }

    #[test]
    fn decodes_gzip() {
        let out = decode_body(
            &headers("gzip"),
            Bytes::from(gzip(b"hello gzip")),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(&out[..], b"hello gzip");
    }

    #[test]
    fn decodes_deflate_both_ways() {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"zlib").unwrap();
        let zlib = enc.finish().unwrap();
        assert_eq!(
            &decode_body(&headers("deflate"), Bytes::from(zlib), usize::MAX).unwrap()[..],
            b"zlib"
        );
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"raw").unwrap();
        let raw = enc.finish().unwrap();
        assert_eq!(
            &decode_body(&headers("deflate"), Bytes::from(raw), usize::MAX).unwrap()[..],
            b"raw"
        );
    }

    #[test]
    fn refuses_bodies_that_decode_past_the_limit() {
        // A megabyte of zeros compresses to about a kilobyte.
        let bomb = gzip(&vec![0u8; 1024 * 1024]);
        assert!(bomb.len() < 4096, "{}", bomb.len());
        match decode_body(&headers("gzip"), Bytes::from(bomb.clone()), 65536) {
            Err(DecodeError::TooLarge(65536)) => {}
            other => panic!("{other:?}"),
        }
        let out = decode_body(&headers("gzip"), Bytes::from(bomb), 1024 * 1024).unwrap();
        assert_eq!(out.len(), 1024 * 1024);
    }

    #[test]
    fn unknown_encodings_fail() {
        assert!(matches!(
            decode_body(&headers("sdch"), Bytes::from_static(b"x"), usize::MAX),
            Err(DecodeError::Failed(_))
        ));
    }
}
