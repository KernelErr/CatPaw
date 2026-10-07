//! The little HTTP the local pages need (the approval page, the hand-off
//! viewer): one request per connection, on 127.0.0.1 only.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?.split(';').find_map(|pair| {
            let (n, v) = pair.trim().split_once('=')?;
            (n == name).then_some(v)
        })
    }
}

pub(crate) fn read_request(stream: &TcpStream) -> std::io::Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or("").to_string();
    let path = words.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
        if headers.len() > 100 {
            break;
        }
    }
    let length = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(16 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What pages without script may load: their own styles and forms.
pub(crate) const FORM_PAGE: &str =
    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'";

/// Answers with a page (or data) that no other site may frame or cache.
pub(crate) fn respond(
    stream: TcpStream,
    status: &str,
    content_type: &str,
    extra: &[String],
    body: &str,
) -> std::io::Result<()> {
    respond_bytes(
        stream,
        status,
        content_type,
        FORM_PAGE,
        extra,
        body.as_bytes(),
    )
}

/// [`respond`] with a content security policy of its own and any bytes.
pub(crate) fn respond_bytes(
    mut stream: TcpStream,
    status: &str,
    content_type: &str,
    csp: &str,
    extra: &[String],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Frame-Options: DENY\r\nReferrer-Policy: same-origin\r\nContent-Security-Policy: {csp}\r\nConnection: close\r\n",
        body.len()
    );
    for line in extra {
        head.push_str(line);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)
}

pub(crate) fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>:root{{color-scheme:light dark}}body{{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}}pre{{white-space:pre-wrap;padding:1rem;border:1px solid #8884;border-radius:.5rem}}button{{font:inherit;padding:.5rem 1.2rem;margin:.25rem .5rem 0 0}}input{{font:inherit;width:100%;padding:.4rem}}small{{opacity:.75}}</style>{body}</html>"
    )
}
