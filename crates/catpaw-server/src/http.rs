//! The little HTTP the local pages need (the approval page, the hand-off
//! viewer): one request per connection, on 127.0.0.1 only.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

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
}

/// The longest request line or header line.
const MAX_LINE: usize = 8 * 1024;
/// The most header lines a request may have.
const MAX_HEADERS: usize = 100;
/// The largest body a request may have.
const MAX_BODY: usize = 16 * 1024;

/// The longest a client may take to send a request: one that sends it a
/// byte at a time does not keep a connection (one of a few) for long.
const REQUEST_TIME: Duration = if cfg!(test) {
    Duration::from_millis(800)
} else {
    Duration::from_secs(10)
};

/// A stream read until a deadline.
struct Deadline<'a> {
    stream: &'a TcpStream,
    until: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the request took too long",
            ));
        }
        self.stream.set_read_timeout(Some(left))?;
        let mut stream = self.stream;
        stream.read(buf)
    }
}

/// One line, `None` when it is longer than [`MAX_LINE`].
fn read_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let read = reader.take(MAX_LINE as u64).read_line(&mut line)?;
    Ok((read < MAX_LINE || line.ends_with('\n')).then_some(line))
}

/// Reads a request; `Ok(Err(status))` for one too large to take, with
/// the status to refuse it with.
pub(crate) fn read_request(stream: &TcpStream) -> std::io::Result<Result<Request, &'static str>> {
    const LONG_HEADERS: &str = "431 Request Header Fields Too Large";
    let mut reader = BufReader::new(Deadline {
        stream,
        until: Instant::now() + REQUEST_TIME,
    });
    let Some(line) = read_line(&mut reader)? else {
        return Ok(Err("414 URI Too Long"));
    };
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or("").to_string();
    let path = words.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    loop {
        let Some(line) = read_line(&mut reader)? else {
            return Ok(Err(LONG_HEADERS));
        };
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if headers.len() == MAX_HEADERS {
            return Ok(Err(LONG_HEADERS));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let length = match headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
    {
        None => 0,
        Some((_, v)) => match v.parse::<usize>() {
            Ok(length) if length <= MAX_BODY => length,
            Ok(_) => return Ok(Err("413 Content Too Large")),
            Err(_) => return Ok(Err("400 Bad Request")),
        },
    };
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Ok(Request {
        method,
        path,
        headers,
        body,
    }))
}

/// Refuses a request that was not read to its end: answers, then reads
/// (and drops) a little of what the client still sends, so that closing
/// does not reset the connection before the client reads the answer.
pub(crate) fn refuse_unread(stream: TcpStream, status: &str) -> std::io::Result<()> {
    let rest = stream.try_clone()?;
    respond(stream, status, "text/plain", &[], "refused\n")?;
    rest.shutdown(std::net::Shutdown::Write)?;
    rest.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
    let mut buf = [0u8; 8192];
    let mut drained = 0;
    while drained < 1 << 20 {
        match (&rest).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
    Ok(())
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
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>:root{{color-scheme:light dark}}body{{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}}pre{{white-space:pre-wrap;padding:1rem;border:1px solid #8884;border-radius:.5rem}}button{{font:inherit;padding:.5rem 1.2rem;margin:.25rem .5rem 0 0}}input:not([type=checkbox]):not([type=radio]){{font:inherit;width:100%;padding:.4rem}}small{{opacity:.75}}</style>{body}</html>"
    )
}
