//! Recording a client's traffic as HAR 1.2 and answering from a recording.
//!
//! A recording holds every hop (redirects are hops of their own) with its
//! decoded body, so that a replay needs no network at all. Requests are
//! matched by method, URL (without the fragment) and a hash of the body;
//! failing that by method and URL without the query (cache-busting and
//! analytics parameters). Identical requests are answered in recorded
//! order, the last answer repeating once they run out.
//!
//! Secrets stay out: request `Cookie`, `Authorization` and signature
//! headers are never written. `Set-Cookie` in responses is kept, since a
//! replayed session needs its cookies.
//!
//! A path ending in `.zst` is written and read zstd-compressed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde_json::{Value, json};
use url::Url;

/// What a client does with a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recording {
    /// Write the traffic to this file (with [`crate::NetClient::save_recording`]).
    Record(PathBuf),
    /// Answer from this file.
    Replay { path: PathBuf, misses: Misses },
}

/// What a replay does with a request the recording has no answer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misses {
    /// Fail the request (a repeatable run).
    Fail,
    /// Go to the network.
    Live,
}

/// Request headers never written.
const SECRET_HEADERS: &[&str] = &[
    "cookie",
    "authorization",
    "proxy-authorization",
    "signature",
    "signature-input",
    "signature-agent",
];

/// The largest body a recording keeps; larger ones replay empty.
const MAX_BODY: usize = 4 * 1024 * 1024;

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn exact_key(method: &str, url: &Url, body: &[u8]) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    format!("{method} {url} {:016x}", fnv(body))
}

fn loose_key(method: &str, url: &Url) -> String {
    format!(
        "{method} {}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        url.path()
    )
}

/// `2026-10-07T12:34:56.789Z` for Unix milliseconds.
fn iso8601(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days to civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

fn read_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    if path.extension().is_some_and(|e| e == "zst") {
        zstd::decode_all(&bytes[..])
    } else {
        Ok(bytes)
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if path.extension().is_some_and(|e| e == "zst") {
        std::fs::write(path, zstd::encode_all(bytes, 19)?)
    } else {
        std::fs::write(path, bytes)
    }
}

fn body_json(body: &[u8], mime: &str) -> Value {
    if body.len() > MAX_BODY {
        return json!({"size": body.len(), "mimeType": mime, "text": "", "_catpawTruncated": true});
    }
    match std::str::from_utf8(body) {
        Ok(text) => json!({"size": body.len(), "mimeType": mime, "text": text}),
        Err(_) => json!({
            "size": body.len(),
            "mimeType": mime,
            "text": base64::engine::general_purpose::STANDARD.encode(body),
            "encoding": "base64",
        }),
    }
}

/// Writes down every hop.
#[derive(Debug)]
pub(crate) struct Recorder {
    path: PathBuf,
    entries: Mutex<Vec<Value>>,
}

impl Recorder {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Mutex::new(Vec::new()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &self,
        method: &Method,
        url: &Url,
        request_headers: &HeaderMap,
        request_body: &[u8],
        status: StatusCode,
        response_headers: &HeaderMap,
        response_body: &[u8],
        started_ms: u64,
        took_ms: u64,
    ) {
        let headers = |map: &HeaderMap, skip: &[&str]| -> Vec<Value> {
            map.iter()
                .filter(|(name, _)| !skip.contains(&name.as_str()))
                .map(|(name, value)| {
                    json!({"name": name.as_str(), "value": String::from_utf8_lossy(value.as_bytes())})
                })
                .collect()
        };
        let mime = response_headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut request = json!({
            "method": method.as_str(),
            "url": url.as_str(),
            "httpVersion": "HTTP/1.1",
            "headers": headers(request_headers, SECRET_HEADERS),
            "queryString": [],
            "cookies": [],
            "headersSize": -1,
            "bodySize": request_body.len(),
        });
        if !request_body.is_empty() {
            let request_mime = request_headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            request["postData"] = json!({
                "mimeType": request_mime,
                "text": String::from_utf8_lossy(&request_body[..request_body.len().min(MAX_BODY)]),
            });
        }
        let entry = json!({
            "startedDateTime": iso8601(started_ms),
            "time": took_ms,
            "request": request,
            "response": {
                "status": status.as_u16(),
                "statusText": status.canonical_reason().unwrap_or(""),
                "httpVersion": "HTTP/1.1",
                "headers": headers(response_headers, &[]),
                "cookies": [],
                "content": body_json(response_body, &mime),
                "redirectURL": response_headers
                    .get(http::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                "headersSize": -1,
                "bodySize": -1,
            },
            "cache": {},
            "timings": {"send": 0, "wait": took_ms, "receive": 0},
            "_catpaw": {"key": exact_key(method.as_str(), url, request_body), "startedMs": started_ms},
        });
        self.entries.lock().expect("not poisoned").push(entry);
    }

    /// Writes the recording so far.
    pub(crate) fn save(&self, user_agent: &str) -> std::io::Result<usize> {
        let entries = self.entries.lock().expect("not poisoned").clone();
        let count = entries.len();
        let har = json!({
            "log": {
                "version": "1.2",
                "creator": {"name": "CatPaw", "version": env!("CARGO_PKG_VERSION")},
                "browser": {"name": "CatPaw", "version": user_agent},
                "entries": entries,
            }
        });
        let text = serde_json::to_string(&har).map_err(std::io::Error::other)?;
        write_file(&self.path, text.as_bytes())?;
        Ok(count)
    }
}

/// A recorded answer.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Answers from a recording.
#[derive(Debug)]
pub(crate) struct Replayer {
    answers: Vec<Answer>,
    exact: HashMap<String, Vec<usize>>,
    loose: HashMap<String, Vec<usize>>,
    /// How many answers of each key were given.
    used: Mutex<HashMap<String, usize>>,
    pub(crate) misses: Misses,
}

impl Replayer {
    pub(crate) fn open(path: &Path, misses: Misses) -> Result<Self, String> {
        let bytes = read_file(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let har: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("parsing {}: {e}", path.display()))?;
        let entries = har["log"]["entries"]
            .as_array()
            .ok_or_else(|| format!("{} has no entries", path.display()))?;
        let mut replayer = Self {
            answers: Vec::with_capacity(entries.len()),
            exact: HashMap::new(),
            loose: HashMap::new(),
            used: Mutex::new(HashMap::new()),
            misses,
        };
        for entry in entries {
            let request = &entry["request"];
            let method = request["method"].as_str().unwrap_or("GET");
            let Some(url) = request["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let request_body = request["postData"]["text"].as_str().unwrap_or("");
            let response = &entry["response"];
            let mut headers = HeaderMap::new();
            for header in response["headers"].as_array().into_iter().flatten() {
                if let (Some(name), Some(value)) =
                    (header["name"].as_str(), header["value"].as_str())
                    && let (Ok(name), Ok(value)) = (
                        HeaderName::from_bytes(name.as_bytes()),
                        HeaderValue::from_str(value),
                    )
                {
                    headers.append(name, value);
                }
            }
            let content = &response["content"];
            let text = content["text"].as_str().unwrap_or("");
            let body = if content["encoding"].as_str() == Some("base64") {
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .unwrap_or_default()
            } else {
                text.as_bytes().to_vec()
            };
            let status = StatusCode::from_u16(response["status"].as_u64().unwrap_or(200) as u16)
                .unwrap_or(StatusCode::OK);
            // Cookie expiry dates were real when recorded; they last as
            // long as they did then.
            if let Some(recorded) = entry["_catpaw"]["startedMs"].as_u64() {
                relative_cookies(&mut headers, recorded);
            }
            let index = replayer.answers.len();
            replayer.answers.push(Answer {
                status,
                headers,
                body: Bytes::from(body),
            });
            replayer
                .exact
                .entry(exact_key(method, &url, request_body.as_bytes()))
                .or_default()
                .push(index);
            replayer
                .loose
                .entry(loose_key(method, &url))
                .or_default()
                .push(index);
        }
        Ok(replayer)
    }

    /// The recorded answer to a request, if there is one.
    pub(crate) fn answer(&self, method: &Method, url: &Url, body: &[u8]) -> Option<Answer> {
        let exact = exact_key(method.as_str(), url, body);
        let (key, list) = match self.exact.get(&exact) {
            Some(list) => (exact, list),
            None => {
                let loose = loose_key(method.as_str(), url);
                let list = self.loose.get(&loose)?;
                (loose, list)
            }
        };
        let mut used = self.used.lock().expect("not poisoned");
        let n = used.entry(key).or_insert(0);
        let index = list[(*n).min(list.len() - 1)];
        *n += 1;
        Some(self.answers[index].clone())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.answers.len()
    }
}

/// `Set-Cookie` headers with their `Expires` turned into a `Max-Age`
/// counted from `recorded_ms`.
fn relative_cookies(headers: &mut HeaderMap, recorded_ms: u64) {
    let values: Vec<HeaderValue> = headers
        .get_all(http::header::SET_COOKIE)
        .iter()
        .map(|value| {
            let text = String::from_utf8_lossy(value.as_bytes()).into_owned();
            let Ok(mut cookie) = cookie::Cookie::parse(text) else {
                return value.clone();
            };
            if cookie.max_age().is_some() {
                return value.clone();
            }
            let Some(expires) = cookie.expires_datetime() else {
                return value.clone();
            };
            let seconds = expires.unix_timestamp() - (recorded_ms / 1000) as i64;
            cookie.unset_expires();
            cookie.set_max_age(cookie::time::Duration::seconds(seconds.max(0)));
            HeaderValue::from_str(&cookie.to_string()).unwrap_or_else(|_| value.clone())
        })
        .collect();
    if values.is_empty() {
        return;
    }
    headers.remove(http::header::SET_COOKIE);
    for value in values {
        headers.append(http::header::SET_COOKIE, value);
    }
}

/// Unix milliseconds now.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_iso() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_791_391_624_223), "2026-10-07T16:47:04.223Z");
    }

    #[test]
    fn recordings_round_trip_and_answer_in_order() {
        let dir = std::env::temp_dir().join(format!("catpaw-har-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.har.zst");
        let recorder = Recorder::new(path.clone());
        let url = Url::parse("https://shop.example/api?x=1#frag").unwrap();
        let mut request_headers = HeaderMap::new();
        request_headers.insert("cookie", HeaderValue::from_static("secret=1"));
        request_headers.insert("accept", HeaderValue::from_static("*/*"));
        let mut response_headers = HeaderMap::new();
        response_headers.insert("content-type", HeaderValue::from_static("text/plain"));
        for body in ["first", "second"] {
            recorder.record(
                &Method::GET,
                &url,
                &request_headers,
                b"",
                StatusCode::OK,
                &response_headers,
                body.as_bytes(),
                1_000,
                5,
            );
        }
        recorder.record(
            &Method::GET,
            &Url::parse("https://shop.example/bin").unwrap(),
            &HeaderMap::new(),
            b"",
            StatusCode::OK,
            &HeaderMap::new(),
            &[0xff, 0x00, 0x80],
            1_000,
            5,
        );
        assert_eq!(recorder.save("test").unwrap(), 3);
        let text = String::from_utf8(read_file(&path).unwrap()).unwrap();
        assert!(!text.contains("secret=1"), "cookies stay out");
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        assert_eq!(replayer.len(), 3);
        let answer = |u: &str| {
            replayer
                .answer(&Method::GET, &Url::parse(u).unwrap(), b"")
                .map(|a| a.body)
        };
        assert_eq!(answer("https://shop.example/api?x=1").unwrap(), "first");
        assert_eq!(
            answer("https://shop.example/api?x=1#other").unwrap(),
            "second"
        );
        // Run out: the last answer repeats; another query: the loose key.
        assert_eq!(answer("https://shop.example/api?x=1").unwrap(), "second");
        assert_eq!(answer("https://shop.example/api?x=2").unwrap(), "first");
        assert_eq!(
            answer("https://shop.example/bin").unwrap().as_ref(),
            &[0xff, 0x00, 0x80]
        );
        assert!(answer("https://shop.example/none").is_none());
        let mut headers = HeaderMap::new();
        headers.append(
            http::header::SET_COOKIE,
            HeaderValue::from_static("s=1; Expires=Thu, 01 Jan 1970 00:10:00 GMT; Path=/"),
        );
        relative_cookies(&mut headers, 0);
        let set = headers
            .get(http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set.contains("Max-Age=600"), "{set}");
        assert!(!set.contains("Expires"), "{set}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
