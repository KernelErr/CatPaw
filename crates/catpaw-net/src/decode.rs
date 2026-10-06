//! `Content-Encoding` decoding for fully buffered bodies.

use std::io::Read;

use bytes::Bytes;
use http::HeaderMap;
use http::header::CONTENT_ENCODING;

/// Decodes `body` according to the response's `Content-Encoding` header.
/// Encodings are listed in the order they were applied, so they are undone
/// in reverse.
pub fn decode_body(headers: &HeaderMap, body: Bytes) -> Result<Bytes, String> {
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
            "gzip" | "x-gzip" => read_all(flate2::read::MultiGzDecoder::new(&data[..]))?,
            // RFC 9110 says zlib-wrapped, but raw DEFLATE streams exist in the wild.
            "deflate" => match read_all(flate2::read::ZlibDecoder::new(&data[..])) {
                Ok(out) => out,
                Err(_) => read_all(flate2::read::DeflateDecoder::new(&data[..]))?,
            },
            "br" => read_all(brotli::Decompressor::new(&data[..], 8192))?,
            "zstd" => read_all(zstd::Decoder::new(&data[..]).map_err(|e| e.to_string())?)?,
            other => return Err(format!("unsupported content-encoding `{other}`")),
        };
    }
    Ok(Bytes::from(data))
}

fn read_all(mut reader: impl Read) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| format!("decompression failed: {e}"))?;
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

    #[test]
    fn passes_identity_through() {
        let body = Bytes::from_static(b"hello");
        assert_eq!(decode_body(&HeaderMap::new(), body.clone()).unwrap(), body);
        assert_eq!(
            decode_body(&headers("identity"), body.clone()).unwrap(),
            body
        );
    }

    #[test]
    fn decodes_gzip() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello gzip").unwrap();
        let compressed = enc.finish().unwrap();
        let out = decode_body(&headers("gzip"), Bytes::from(compressed)).unwrap();
        assert_eq!(&out[..], b"hello gzip");
    }

    #[test]
    fn decodes_brotli() {
        let mut compressed = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            w.write_all(b"hello brotli").unwrap();
        }
        let out = decode_body(&headers("br"), Bytes::from(compressed)).unwrap();
        assert_eq!(&out[..], b"hello brotli");
    }

    #[test]
    fn decodes_zstd() {
        let compressed = zstd::encode_all(&b"hello zstd"[..], 3).unwrap();
        let out = decode_body(&headers("zstd"), Bytes::from(compressed)).unwrap();
        assert_eq!(&out[..], b"hello zstd");
    }

    #[test]
    fn rejects_unknown_encoding() {
        assert!(decode_body(&headers("compress"), Bytes::from_static(b"x")).is_err());
    }
}
