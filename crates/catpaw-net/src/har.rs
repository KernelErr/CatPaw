//! Recording a client's traffic as HAR 1.2 and answering from a recording.
//!
//! A recording holds every hop (redirects are hops of their own) with its
//! decoded body, whatever its size, so that a replay needs no network at
//! all. Entries are written in the order the requests were made, each
//! request's hops together: a request takes its place when it is made
//! ([`crate::NetClient::reserve_place`]), whichever response comes first,
//! and that is the order a replay asks for them in. Bodies that are not
//! UTF-8 are kept in base64 (`"encoding": "base64"`, request bodies too).
//!
//! A request is matched by the key recorded with each entry
//! (`_catpaw.key`: method, URL without the fragment, and a hash of the
//! body, URL and body as the recording holds them), failing that by
//! method, scheme, host, port and path (cache-busting and analytics
//! parameters). A replayed request's key is computed the same way, its
//! secrets redacted first. Every recorded answer is given once, in
//! recorded order, whichever way it was matched; once all the answers a
//! request matches have been given, the last of them repeats.
//!
//! Secrets stay out. Request `Cookie`, `Authorization`, signature and other
//! credential headers are never written. Fields whose names look secret
//! ([`is_secret_field`]) are written as `redacted`: in URL-encoded,
//! plain-text, multipart and JSON request bodies, and as parameters in the
//! query or fragment of request URLs, `Referer` and `Location`. The values
//! of response cookies, credential headers and secret-looking fields of
//! JSON responses become placeholders, `redacted-<hash>`: within one
//! recording equal values get equal placeholders, and a replayed session
//! keeps and sends them back as it would the real ones. Files are readable
//! by their owner only.
//!
//! A path ending in `.zst` is written and read zstd-compressed.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasher;
use std::io::Write as _;
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

/// What a secret form field holds in a recording.
pub const REDACTED: &str = "redacted";

/// Headers that carry credentials, by their whole (lowercase) name.
const CREDENTIAL_HEADERS: &[&str] = &[
    "cookie",
    "set-cookie",
    "set-cookie2",
    "authorization",
    "proxy-authorization",
    "authentication-info",
    "proxy-authentication-info",
    "signature",
    "signature-input",
    "signature-agent",
];

/// Parts of header names that carry credentials (`x-csrf-token`,
/// `x-api-key`, `x-session-id`).
const CREDENTIAL_HEADER_PARTS: &[&str] = &[
    "token", "secret", "csrf", "xsrf", "api-key", "apikey", "password", "session",
];

/// Parts that make a word of a name hold a secret wherever they stand in
/// it (`newpassword`, `passwd`, `pwd2`, `clientsecret`).
const SECRET_PARTS: &[&str] = &["passw", "pwd", "passphrase", "passcode", "secret"];

/// Endings that make a word hold a secret (`pass`, `userpass`, `pw`,
/// `csrftoken`, `totp`, `mpin`).
const SECRET_ENDINGS: &[&str] = &["pass", "pw", "token", "otp", "pin"];

/// Words that hold a secret by themselves.
const SECRET_WORDS: &[&str] = &[
    "card",
    "creditcard",
    "debitcard",
    "giftcard",
    "cvv",
    "cvc",
    "csc",
    "ssn",
    "pincode",
    "authorization",
    "authorisation",
    "verifier",
];

/// Nouns that hold a secret after one of their words (`api_key`,
/// `verification_code`, `cc_number`, `session_id`), written apart or as
/// one word (`apikey`, `authcode`).
const QUALIFIED_SECRETS: &[(&str, &[&str])] = &[
    (
        "key",
        &[
            "api",
            "access",
            "private",
            "client",
            "session",
            "auth",
            "signing",
            "encryption",
            "license",
            "licence",
            "activation",
            "recovery",
            "master",
            "subscription",
        ],
    ),
    (
        "code",
        &[
            "onetime",
            "mfa",
            "2fa",
            "tfa",
            "twofactor",
            "multifactor",
            "verification",
            "verify",
            "auth",
            "authentication",
            "security",
            "sms",
            "recovery",
            "backup",
            "reset",
            "login",
            "signin",
            "access",
            "activation",
            "confirmation",
            "confirm",
            "unlock",
        ],
    ),
    ("number", &["card", "cc", "pin", "socialsecurity"]),
    ("num", &["card", "cc", "pin"]),
    ("id", &["session", "sess", "phpsess", "jsession"]),
];

/// Names that are a one-time code when they are the whole name (`mfa`,
/// `2fa`), though not as a word of a longer one (`mfa_enabled`).
const SECRET_NAMES: &[&str] = &["mfa", "2fa", "tfa"];

/// Words that, after the one that names a secret, make the name describe
/// the secret rather than hold it (`token_type`, `pin_length`,
/// `api_key_id`).
const DESCRIPTIONS: &[&str] = &[
    "type",
    "endpoint",
    "url",
    "uri",
    "expires",
    "expiry",
    "ttl",
    "lifetime",
    "length",
    "len",
    "min",
    "max",
    "policy",
    "strength",
    "hint",
    "format",
    "required",
    "supported",
    "method",
    "methods",
    "count",
    "id",
    "label",
    "brand",
    "last",
];

/// Whether a header (by its lowercase name) carries credentials. Requests
/// are recorded without them; in responses their values become
/// placeholders.
fn is_credential_header(name: &str) -> bool {
    CREDENTIAL_HEADERS.contains(&name) || CREDENTIAL_HEADER_PARTS.iter().any(|p| name.contains(p))
}

/// Whether a header (by its lowercase name) holds a URL whose parameters
/// a recording redacts.
fn is_url_header(name: &str) -> bool {
    matches!(name, "referer" | "location" | "content-location")
}

/// Whether the name of a form field, a URL parameter or a JSON member
/// says it holds a secret, by its words in any case (`cardPIN2` is `card`,
/// `pin`, `2`): passwords (`password`, `passwd`, `passphrase`, `passcode`,
/// `pwd`, `pw`, `pass` and words ending so), secrets and tokens
/// (`client_secret`, `access_token`, `csrfmiddlewaretoken`), keys
/// (`api_key`, `x-api-key`, `apiKey`, `access_key`, `private_key`),
/// one-time codes (`otp`, `totp`, `mfa`, `2fa_code`, `one_time_code`,
/// `verification_code`, `auth_code`, `security_code`), PINs (`pin`,
/// `pinCode`, `pincode`, `mpin`), card numbers and their codes (`card`,
/// `cardNumber`, `cc_number`, `cvv`, `cvc`), social security numbers and
/// session ids. A word naming a secret that is followed by one describing
/// it does not count (`token_type`, `pin_length`, `access_key_id`). Other
/// codes are not secrets (`country_code`, `zip_code`, `postcode`,
/// `promo_code`), nor are words that only contain a secret's name
/// (`shipping`, `spinner`, `passenger`, `className`, `rootPath`).
pub fn is_secret_field(name: &str) -> bool {
    let words: Vec<String> = words(name)
        .iter()
        .map(|word| word.to_ascii_lowercase())
        .collect();
    SECRET_NAMES.contains(&words.concat().as_str())
        || (0..words.len()).any(|i| {
            names_secret(&words, i)
                && !words
                    .get(i + 1)
                    .is_some_and(|next| DESCRIPTIONS.contains(&next.as_str()))
        })
}

/// Whether word `i` of a name (its words in lowercase), with the words
/// before it, names a secret.
fn names_secret(words: &[String], i: usize) -> bool {
    let word = words[i].as_str();
    let named = (SECRET_PARTS.iter().any(|part| word.contains(part))
        && !word.starts_with("secretar"))
        || (SECRET_ENDINGS.iter().any(|end| word.ends_with(end)) && !word.ends_with("spin"))
        || word.starts_with("otp")
        || SECRET_WORDS.contains(&word);
    named
        || QUALIFIED_SECRETS.iter().any(|(noun, qualifiers)| {
            let Some(start) = word.strip_suffix(noun) else {
                return false;
            };
            // The qualifier is the start of the word, the words before
            // it, or both (`authcode`, `api_key`, `one_time_code`).
            let mut qualifier = start.to_string();
            (!start.is_empty() && qualifiers.contains(&start))
                || words[..i].iter().rev().take(2).any(|before| {
                    qualifier.insert_str(0, before);
                    qualifiers.contains(&qualifier.as_str())
                })
        })
}

/// The words of an identifier: runs of letters and digits, split where
/// the case or the kind of character changes (`cardPIN2` is `card`,
/// `PIN`, `2`).
fn words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some(&before) = i.checked_sub(1).and_then(|j| chars.get(j)) {
            let after = chars.get(i + 1).copied();
            let boundary = (c.is_uppercase() && (before.is_lowercase() || before.is_numeric()))
                || (c.is_uppercase()
                    && before.is_uppercase()
                    && after.is_some_and(char::is_lowercase))
                || (c.is_numeric() != before.is_numeric() && before.is_alphanumeric());
            if boundary && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Whether a field's value still needs hiding: an empty one has nothing
/// to hide, and a redacted one is hidden already.
fn needs_redaction(value: &[u8]) -> bool {
    !value.is_empty() && value != REDACTED.as_bytes()
}

/// A body with its secret fields redacted, and the names of those fields.
struct Redaction {
    body: Vec<u8>,
    fields: Vec<String>,
}

/// The body with the values of its secret-looking fields replaced, or
/// `None` when it holds none (or they are hidden already). Forms
/// (URL-encoded, plain-text and multipart) and JSON are understood; a body
/// without a type is taken for JSON or a URL-encoded form when it looks
/// like one.
fn redact_body(mime: &str, body: &[u8]) -> Option<Redaction> {
    let essence = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "application/x-www-form-urlencoded" => redact_urlencoded(body),
        "multipart/form-data" => redact_multipart(body, &boundary_of(mime)?),
        // `fetch()` sends a string body as text/plain: often JSON.
        "text/plain" => redact_json(body).or_else(|| redact_plain_form(body)),
        "" => redact_json(body).or_else(|| {
            let form_like = body.iter().all(u8::is_ascii_graphic) && body.contains(&b'=');
            if form_like {
                redact_urlencoded(body)
            } else {
                None
            }
        }),
        json if json.ends_with("/json") || json.ends_with("+json") => redact_json(body),
        _ => None,
    }
}

/// `a=1&password=x` → `a=1&password=redacted`, the rest byte for byte.
fn redact_urlencoded(body: &[u8]) -> Option<Redaction> {
    let mut out = Vec::with_capacity(body.len());
    let mut fields = Vec::new();
    for (i, pair) in body.split(|b| *b == b'&').enumerate() {
        if i > 0 {
            out.push(b'&');
        }
        if let Some(at) = pair.iter().position(|b| *b == b'=') {
            let (name, value) = (&pair[..at], &pair[at + 1..]);
            let decoded = url::form_urlencoded::parse(name)
                .next()
                .map(|(name, _)| name.into_owned())
                .unwrap_or_default();
            if is_secret_field(&decoded) && needs_redaction(value) {
                out.extend_from_slice(name);
                out.push(b'=');
                out.extend_from_slice(REDACTED.as_bytes());
                fields.push(decoded);
                continue;
            }
        }
        out.extend_from_slice(pair);
    }
    (!fields.is_empty()).then_some(Redaction { body: out, fields })
}

/// A `text/plain` form: `name=value` lines.
fn redact_plain_form(body: &[u8]) -> Option<Redaction> {
    let mut out = Vec::with_capacity(body.len());
    let mut fields = Vec::new();
    for (i, line) in body.split(|b| *b == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        let (line, cr) = match line.strip_suffix(b"\r") {
            Some(line) => (line, true),
            None => (line, false),
        };
        match line.iter().position(|b| *b == b'=') {
            Some(at) if needs_redaction(&line[at + 1..]) => {
                let name = String::from_utf8_lossy(&line[..at]).into_owned();
                if is_secret_field(&name) {
                    out.extend_from_slice(&line[..=at]);
                    out.extend_from_slice(REDACTED.as_bytes());
                    fields.push(name);
                } else {
                    out.extend_from_slice(line);
                }
            }
            _ => out.extend_from_slice(line),
        }
        if cr {
            out.push(b'\r');
        }
    }
    (!fields.is_empty()).then_some(Redaction { body: out, fields })
}

/// JSON with the values under secret-looking keys replaced (strings and
/// numbers, at any depth below such a key).
fn redact_json(body: &[u8]) -> Option<Redaction> {
    let first = body.iter().find(|b| !b.is_ascii_whitespace())?;
    if !matches!(first, b'{' | b'[') {
        return None;
    }
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let mut fields = Vec::new();
    redact_json_value(&mut value, None, &mut fields);
    if fields.is_empty() {
        return None;
    }
    fields.sort();
    fields.dedup();
    let body = serde_json::to_vec(&value).ok()?;
    Some(Redaction { body, fields })
}

/// Redacts below `secret` (the secret key this value is under, if any).
fn redact_json_value(value: &mut Value, secret: Option<&str>, fields: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let under = secret.or(is_secret_field(key).then_some(key.as_str()));
                redact_json_value(item, under, fields);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json_value(item, secret, fields);
            }
        }
        Value::String(text) if secret.is_some() && needs_redaction(text.as_bytes()) => {
            *text = REDACTED.to_string();
            fields.extend(secret.map(str::to_string));
        }
        Value::Number(_) if secret.is_some() => {
            *value = Value::String(REDACTED.to_string());
            fields.extend(secret.map(str::to_string));
        }
        _ => {}
    }
}

/// The `boundary` parameter of a multipart type.
fn boundary_of(mime: &str) -> Option<String> {
    mime.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| value.trim().trim_matches('"').to_string())
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The `name` of a part, from its `Content-Disposition` header.
fn disposition_name(headers: &[u8]) -> Option<String> {
    let headers = String::from_utf8_lossy(headers);
    let line = headers.split("\r\n").find(|line| {
        line.split_once(':')
            .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("content-disposition"))
    })?;
    let (_, value) = line.split_once(':')?;
    value.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("name")
            .then(|| value.trim().trim_matches('"').to_string())
    })
}

/// A multipart body with the contents of its secret-looking parts
/// replaced, everything else byte for byte.
fn redact_multipart(body: &[u8], boundary: &str) -> Option<Redaction> {
    let delimiter = format!("--{boundary}").into_bytes();
    let mut positions = Vec::new();
    let mut from = 0;
    while let Some(at) = find(&body[from..], &delimiter) {
        positions.push(from + at);
        from += at + delimiter.len();
    }
    let mut out = Vec::with_capacity(body.len());
    let mut fields = Vec::new();
    let mut copied = 0;
    for pair in positions.windows(2) {
        let (start, next) = (pair[0] + delimiter.len(), pair[1]);
        // The part starts after the delimiter's line and ends before the
        // line break that precedes the next delimiter.
        let Some(eol) = find(&body[start..next], b"\r\n") else {
            continue;
        };
        let part_start = start + eol + 2;
        let part_end = if body[..next].ends_with(b"\r\n") {
            next - 2
        } else {
            next
        };
        if part_start > part_end {
            continue;
        }
        let Some(split) = find(&body[part_start..part_end], b"\r\n\r\n") else {
            continue;
        };
        let content_start = part_start + split + 4;
        let Some(name) = disposition_name(&body[part_start..part_start + split]) else {
            continue;
        };
        if is_secret_field(&name) && needs_redaction(&body[content_start..part_end]) {
            out.extend_from_slice(&body[copied..content_start]);
            out.extend_from_slice(REDACTED.as_bytes());
            copied = part_end;
            fields.push(name);
        }
    }
    if fields.is_empty() {
        return None;
    }
    out.extend_from_slice(&body[copied..]);
    Some(Redaction { body: out, fields })
}

/// Whether a value is a placeholder a recording wrote: `redacted-` and
/// eight hexadecimal digits.
fn is_placeholder(value: &str) -> bool {
    value
        .strip_prefix("redacted-")
        .is_some_and(|hash| hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The value of a `Set-Cookie` header (between the `=` and the first
/// `;`), and the name before it.
fn cookie_pair(set_cookie: &str) -> (&str, &str) {
    let pair = set_cookie.split(';').next().unwrap_or("");
    match pair.split_once('=') {
        Some((name, value)) => (name.trim(), value.trim()),
        None => ("", pair.trim()),
    }
}

/// FNV-1a (64 bits). It has no key, so it only ever hashes what a
/// recording holds anyway (see [`request_key`]).
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// A URL (as text, absolute or relative) with the values of its
/// secret-looking parameters redacted, in the query and in a fragment of
/// `name=value` pairs, the rest byte for byte; `None` when it has none to
/// redact. The names of the parameters redacted come with it.
fn redact_url(url: &str) -> Option<(String, Vec<String>)> {
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (url, None),
    };
    let (base, query) = match rest.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (rest, None),
    };
    let redacted = [
        query.and_then(|query| redact_urlencoded(query.as_bytes())),
        fragment
            .filter(|fragment| fragment.contains('='))
            .and_then(|fragment| redact_urlencoded(fragment.as_bytes())),
    ];
    if redacted.iter().all(Option::is_none) {
        return None;
    }
    let mut out = base.to_string();
    let mut fields = Vec::new();
    for ((separator, part), redaction) in [('?', query), ('#', fragment)].into_iter().zip(redacted)
    {
        let Some(part) = part else { continue };
        out.push(separator);
        match redaction {
            Some(redaction) => {
                out.push_str(&String::from_utf8_lossy(&redaction.body));
                fields.extend(redaction.fields);
            }
            None => out.push_str(part),
        }
    }
    Some((out, fields))
}

/// The key a request is matched by: its method, its URL without the
/// fragment and a hash of its body, URL and body as a recording holds
/// them (secrets redacted), so that a key tells nothing the recording
/// does not.
fn request_key(method: &str, url: &Url, mime: &str, body: &[u8]) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    let url = match redact_url(url.as_str()) {
        Some((redacted, _)) => redacted,
        None => url.into(),
    };
    let redaction = redact_body(mime, body);
    let body = redaction.as_ref().map_or(body, |r| &r.body[..]);
    format!("{method} {url} {:016x}", fnv(body))
}

/// The key of a recorded request, from the request as the recording holds
/// it; `None` when its URL does not parse.
fn recorded_key(request: &Value) -> Result<Option<String>, String> {
    let method = request["method"].as_str().unwrap_or("GET");
    let Some(url) = request["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
        return Ok(None);
    };
    let (mime, body) = match request.get("postData") {
        Some(post) => (post["mimeType"].as_str().unwrap_or(""), decode_text(post)?),
        None => ("", Vec::new()),
    };
    Ok(Some(request_key(method, &url, mime, &body)))
}

fn loose_key(method: &str, url: &Url) -> String {
    let port = url
        .port_or_known_default()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!(
        "{method} {}://{}{port}{}",
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

/// Writes `bytes`, compressed for a `.zst` path, to a file only its owner
/// may read.
fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let bytes = if path.extension().is_some_and(|e| e == "zst") {
        Cow::Owned(zstd::encode_all(bytes, 19)?)
    } else {
        Cow::Borrowed(bytes)
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // A file that was there already keeps its mode through `open`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&bytes)?;
    file.flush()
}

fn base64_text(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A response body as HAR content: text, or base64 when not UTF-8.
fn content_json(body: &[u8], mime: &str) -> Value {
    match std::str::from_utf8(body) {
        Ok(text) => json!({"size": body.len(), "mimeType": mime, "text": text}),
        Err(_) => json!({
            "size": body.len(),
            "mimeType": mime,
            "text": base64_text(body),
            "encoding": "base64",
        }),
    }
}

/// JSON fields in which services echo the address a request came from
/// (httpbin's `origin`, "what is my IP" services, proxies' headers).
const ADDRESS_FIELDS: &[&str] = &[
    "origin",
    "ip",
    "client_ip",
    "clientip",
    "remote_addr",
    "remoteaddr",
    "x-forwarded-for",
    "x-real-ip",
];

/// What a recording keeps in place of an echoed address: one reserved for
/// documentation (RFC 5737), never the recording machine's.
pub const STAND_IN_ADDRESS: &str = "203.0.113.7";

/// The addresses a JSON body echoes in [`ADDRESS_FIELDS`] (comma lists
/// included), other than the stand-in.
fn echoed_addresses(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if ADDRESS_FIELDS.contains(&key.to_ascii_lowercase().as_str())
                    && let Some(text) = value.as_str()
                {
                    for part in text.split(',').map(str::trim) {
                        if part.parse::<std::net::IpAddr>().is_ok() && part != STAND_IN_ADDRESS {
                            out.push(part.to_string());
                        }
                    }
                }
                echoed_addresses(value, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| echoed_addresses(item, out)),
        _ => {}
    }
}

fn is_json(mime: &str) -> bool {
    mime.to_ascii_lowercase().contains("json")
}

/// A JSON response body with the addresses it echoes replaced by the
/// stand-in, everywhere in the body; `None` when it echoes none.
fn scrub_addresses(body: &[u8], mime: &str) -> Option<Vec<u8>> {
    if !is_json(mime) {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let mut found = Vec::new();
    echoed_addresses(&value, &mut found);
    if found.is_empty() {
        return None;
    }
    let mut text = String::from_utf8(body.to_vec()).ok()?;
    for address in found {
        text = text.replace(&address, STAND_IN_ADDRESS);
    }
    Some(text.into_bytes())
}

/// Whether a secret's value is hidden already, or there is nothing to
/// hide: empty, `redacted`, or a placeholder.
fn is_hidden(value: &str) -> bool {
    value.is_empty() || value == REDACTED || is_placeholder(value)
}

/// A value a JSON text holds under a secret-looking name.
struct JsonSecret {
    /// Where it is in the text: a string with its quotes, or a number.
    at: std::ops::Range<usize>,
    /// The string, or the number as written.
    value: String,
    /// The name it is under.
    name: String,
}

/// The values a JSON text holds under secret-looking names: strings and
/// numbers, at any depth below such a name. Empty when the text is not
/// JSON.
fn json_secrets(text: &[u8]) -> Vec<JsonSecret> {
    /// An object or an array the scan is in.
    struct Container {
        object: bool,
        /// Whether a member's name comes next.
        expects_name: bool,
        /// The name of the member being read.
        name: Option<String>,
        /// The secret-looking name the container is under.
        under: Option<String>,
    }
    /// The secret-looking name a value read now is under.
    fn under(stack: &[Container]) -> Option<String> {
        let top = stack.last()?;
        top.under.clone().or_else(|| {
            top.name
                .clone()
                .filter(|name| top.object && is_secret_field(name))
        })
    }
    if serde_json::from_slice::<serde::de::IgnoredAny>(text).is_err() {
        return Vec::new();
    }
    let mut stack: Vec<Container> = Vec::new();
    let mut found = Vec::new();
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            open @ (b'{' | b'[') => {
                let under = under(&stack);
                stack.push(Container {
                    object: open == b'{',
                    expects_name: open == b'{',
                    name: None,
                    under,
                });
                i += 1;
            }
            b'}' | b']' => {
                stack.pop();
                i += 1;
            }
            b',' => {
                if let Some(top) = stack.last_mut() {
                    top.expects_name = top.object;
                }
                i += 1;
            }
            b'"' => {
                // The string ends at the first quote that is not escaped.
                let mut end = i + 1;
                while end < text.len() && text[end] != b'"' {
                    end += if text[end] == b'\\' { 2 } else { 1 };
                }
                let at = i..(end + 1).min(text.len());
                i = at.end;
                let string =
                    || serde_json::from_slice::<String>(&text[at.clone()]).unwrap_or_default();
                if let Some(top) = stack.last_mut().filter(|top| top.expects_name) {
                    top.name = Some(string());
                    top.expects_name = false;
                } else if let Some(name) = under(&stack) {
                    let value = string();
                    found.push(JsonSecret { at, value, name });
                }
            }
            b'-' | b'0'..=b'9' => {
                let end = text[i..]
                    .iter()
                    .position(|b| !matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
                    .map_or(text.len(), |n| i + n);
                if let Some(name) = under(&stack) {
                    found.push(JsonSecret {
                        at: i..end,
                        value: String::from_utf8_lossy(&text[i..end]).into_owned(),
                        name,
                    });
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    found
}

/// A request body as HAR `postData`: text, or base64 when not UTF-8.
fn post_data_json(body: &[u8], mime: &str) -> Value {
    match std::str::from_utf8(body) {
        Ok(text) => json!({"mimeType": mime, "text": text}),
        Err(_) => json!({"mimeType": mime, "text": base64_text(body), "encoding": "base64"}),
    }
}

/// The bytes of a HAR `text`, decoded per its `encoding`.
fn decode_text(holder: &Value) -> Result<Vec<u8>, String> {
    let text = holder["text"].as_str().unwrap_or("");
    match holder["encoding"].as_str() {
        Some("base64") => base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|e| format!("its base64 body does not decode: {e}")),
        _ => Ok(text.as_bytes().to_vec()),
    }
}

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// One request and its response, as the client saw them.
pub(crate) struct Exchange<'a> {
    pub method: &'a Method,
    pub url: &'a Url,
    pub request_headers: &'a HeaderMap,
    /// The body as it was sent.
    pub request_body: &'a [u8],
    pub status: StatusCode,
    pub response_headers: &'a HeaderMap,
    /// The decoded body.
    pub response_body: &'a [u8],
    pub started_ms: u64,
    pub took_ms: u64,
}

/// A request's place in a recording: its entries, one per hop, go there
/// whenever it is sent and answered (see
/// [`crate::NetClient::reserve_place`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Place(u64);

/// Writes down every hop.
#[derive(Debug)]
pub(crate) struct Recorder {
    path: PathBuf,
    state: Mutex<RecorderState>,
    /// Keys the placeholders of secret values: equal values get equal
    /// placeholders within the recording, and nothing about a value can be
    /// learnt from its placeholder.
    keys: std::hash::RandomState,
}

#[derive(Debug, Default)]
struct RecorderState {
    /// The place the next request takes.
    next: u64,
    /// The finished exchanges, by the place of their request and their
    /// hop.
    entries: BTreeMap<(Place, usize), Value>,
}

impl Recorder {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(RecorderState::default()),
            keys: std::hash::RandomState::new(),
        }
    }

    /// The next place, for a request about to be made.
    pub(crate) fn reserve(&self) -> Place {
        let mut state = self.state.lock().expect("not poisoned");
        let place = Place(state.next);
        state.next += 1;
        place
    }

    /// `redacted-<hash>` for a secret value.
    fn placeholder(&self, value: &str) -> String {
        format!("redacted-{:08x}", self.keys.hash_one(value) as u32)
    }

    /// A JSON response body with the values under its secret-looking names
    /// replaced by placeholders, the rest byte for byte; `None` when it
    /// holds none to hide.
    fn redact_json_response(&self, body: &[u8]) -> Option<Vec<u8>> {
        let secrets: Vec<JsonSecret> = json_secrets(body)
            .into_iter()
            .filter(|secret| !is_hidden(&secret.value))
            .collect();
        if secrets.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(body.len());
        let mut copied = 0;
        for secret in secrets {
            out.extend_from_slice(&body[copied..secret.at.start]);
            out.extend_from_slice(format!("\"{}\"", self.placeholder(&secret.value)).as_bytes());
            copied = secret.at.end;
        }
        out.extend_from_slice(&body[copied..]);
        Some(out)
    }

    /// A `Set-Cookie` value with the cookie's value replaced by a
    /// placeholder (an empty one, which deletes, is kept), its name and
    /// attributes as they were.
    fn redact_set_cookie(&self, set_cookie: &str) -> String {
        let (pair, attributes) = match set_cookie.find(';') {
            Some(at) => set_cookie.split_at(at),
            None => (set_cookie, ""),
        };
        let (name, value) = cookie_pair(pair);
        if value.is_empty() {
            return set_cookie.to_string();
        }
        let placeholder = self.placeholder(value);
        if pair.contains('=') {
            format!("{name}={placeholder}{attributes}")
        } else {
            format!("{placeholder}{attributes}")
        }
    }

    /// Writes down hop `hop` (counting from 0) of the request at `place`.
    pub(crate) fn record(&self, place: Place, hop: usize, exchange: Exchange<'_>) {
        let Exchange {
            method,
            url,
            request_headers,
            request_body,
            status,
            response_headers,
            response_body,
            started_ms,
            took_ms,
        } = exchange;
        // URLs keep their parameters, but for the values of secret ones.
        let text = |name: &HeaderName, value: &HeaderValue| {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            if !is_url_header(name.as_str()) {
                return value;
            }
            redact_url(&value).map_or(value, |(redacted, _)| redacted)
        };
        let request_header_list: Vec<Value> = request_headers
            .iter()
            .filter(|(name, _)| !is_credential_header(name.as_str()))
            .map(|(name, value)| json!({"name": name.as_str(), "value": text(name, value)}))
            .collect();
        let response_header_list: Vec<Value> = response_headers
            .iter()
            .map(|(name, value)| {
                let value = text(name, value);
                let value = match name.as_str() {
                    "set-cookie" | "set-cookie2" => self.redact_set_cookie(&value),
                    other if is_credential_header(other) && !value.is_empty() => {
                        self.placeholder(&value)
                    }
                    _ => value,
                };
                json!({"name": name.as_str(), "value": value})
            })
            .collect();
        let redirect_url = response_headers
            .get(http::header::LOCATION)
            .map(|value| text(&http::header::LOCATION, value))
            .unwrap_or_default();
        let request_mime = content_type(request_headers);
        let mut request = json!({
            "method": method.as_str(),
            "url": redact_url(url.as_str()).map_or_else(|| url.to_string(), |(redacted, _)| redacted),
            "httpVersion": "HTTP/1.1",
            "headers": request_header_list,
            "queryString": [],
            "cookies": [],
            "headersSize": -1,
            "bodySize": request_body.len(),
        });
        if !request_body.is_empty() {
            let stored = match redact_body(request_mime, request_body) {
                Some(redaction) => Cow::Owned(redaction.body),
                None => Cow::Borrowed(request_body),
            };
            request["postData"] = post_data_json(&stored, request_mime);
        }
        // Secrets the service hands out are kept as placeholders; an
        // address it echoes is the recording machine's.
        let mime = content_type(response_headers);
        let redacted = if is_json(mime) {
            self.redact_json_response(response_body)
        } else {
            None
        };
        let body = redacted.as_deref().unwrap_or(response_body);
        let content = match scrub_addresses(body, mime) {
            Some(scrubbed) => content_json(&scrubbed, mime),
            None => content_json(body, mime),
        };
        let entry = json!({
            "startedDateTime": iso8601(started_ms),
            "time": took_ms,
            "request": request,
            "response": {
                "status": status.as_u16(),
                "statusText": status.canonical_reason().unwrap_or(""),
                "httpVersion": "HTTP/1.1",
                "headers": response_header_list,
                "cookies": [],
                "content": content,
                "redirectURL": redirect_url,
                "headersSize": -1,
                "bodySize": -1,
            },
            "cache": {},
            "timings": {"send": 0, "wait": took_ms, "receive": 0},
            // From the URL and body as written above, secrets redacted.
            "_catpaw": {
                "key": request_key(method.as_str(), url, request_mime, request_body),
                "startedMs": started_ms,
            },
        });
        self.state
            .lock()
            .expect("not poisoned")
            .entries
            .insert((place, hop), entry);
    }

    /// Writes the recording so far.
    pub(crate) fn save(&self, user_agent: &str) -> std::io::Result<usize> {
        let entries: Vec<Value> = self
            .state
            .lock()
            .expect("not poisoned")
            .entries
            .values()
            .cloned()
            .collect();
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
    /// Each entry's answer, or why the recording cannot give it.
    answers: Vec<Result<Answer, String>>,
    exact: HashMap<String, Vec<usize>>,
    loose: HashMap<String, Vec<usize>>,
    /// Which entries have been given.
    used: Mutex<Vec<bool>>,
    pub(crate) misses: Misses,
}

/// An entry's answer.
fn answer_of(entry: &Value, what: &str) -> Result<Answer, String> {
    let response = &entry["response"];
    let mut headers = HeaderMap::new();
    for header in response["headers"].as_array().into_iter().flatten() {
        if let (Some(name), Some(value)) = (header["name"].as_str(), header["value"].as_str())
            && let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            )
        {
            headers.append(name, value);
        }
    }
    let content = &response["content"];
    // Recordings made before bodies were kept whole left out large ones.
    if content["_catpawTruncated"].as_bool() == Some(true) {
        return Err(format!(
            "the recording left out the {}-byte body of {what}",
            content["size"].as_u64().unwrap_or(0)
        ));
    }
    let body = decode_text(content).map_err(|e| format!("{what}: {e}"))?;
    let status = StatusCode::from_u16(response["status"].as_u64().unwrap_or(200) as u16)
        .unwrap_or(StatusCode::OK);
    // Cookie expiry dates were real when recorded; they last as long as
    // they did then.
    if let Some(recorded) = entry["_catpaw"]["startedMs"].as_u64() {
        relative_cookies(&mut headers, recorded);
    }
    Ok(Answer {
        status,
        headers,
        body: Bytes::from(body),
    })
}

impl Replayer {
    pub(crate) fn open(path: &Path, misses: Misses) -> Result<Self, String> {
        let bytes = read_file(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Self::from_bytes(&bytes, misses).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn from_bytes(bytes: &[u8], misses: Misses) -> Result<Self, String> {
        let har: Value = serde_json::from_slice(bytes).map_err(|e| format!("parsing: {e}"))?;
        let entries = har["log"]["entries"]
            .as_array()
            .ok_or_else(|| "no entries".to_string())?;
        let mut replayer = Self {
            answers: Vec::with_capacity(entries.len()),
            exact: HashMap::new(),
            loose: HashMap::new(),
            used: Mutex::new(Vec::new()),
            misses,
        };
        for entry in entries {
            let request = &entry["request"];
            let method = request["method"].as_str().unwrap_or("GET");
            let Some(url) = request["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let what = format!("{method} {url}");
            // The key the recording computed; a HAR file from elsewhere
            // has the request itself.
            let key = match entry["_catpaw"]["key"].as_str() {
                Some(key) => key.to_string(),
                None => {
                    let (mime, body) = match request.get("postData") {
                        Some(post) => (
                            post["mimeType"].as_str().unwrap_or(""),
                            decode_text(post).unwrap_or_default(),
                        ),
                        None => ("", Vec::new()),
                    };
                    request_key(method, &url, mime, &body)
                }
            };
            let index = replayer.answers.len();
            replayer.answers.push(answer_of(entry, &what));
            replayer.exact.entry(key).or_default().push(index);
            replayer
                .loose
                .entry(loose_key(method, &url))
                .or_default()
                .push(index);
        }
        *replayer.used.get_mut().expect("not poisoned") = vec![false; replayer.answers.len()];
        Ok(replayer)
    }

    /// The recorded answer to a request (its body of type `mime`): the
    /// first not yet given of those recorded for the same key, else for
    /// the same path; once all have been given, the last of them again.
    /// `None` when nothing matches; an error when the recording cannot
    /// give the answer it holds.
    pub(crate) fn answer(
        &self,
        method: &Method,
        url: &Url,
        mime: &str,
        body: &[u8],
    ) -> Option<Result<Answer, String>> {
        let list = self
            .exact
            .get(&request_key(method.as_str(), url, mime, body))
            .or_else(|| self.loose.get(&loose_key(method.as_str(), url)))?;
        let mut used = self.used.lock().expect("not poisoned");
        let index = list
            .iter()
            .copied()
            .find(|&i| !used[i])
            .or_else(|| list.last().copied())?;
        used[index] = true;
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

/// A recorded request as reports name it: its method and its URL, secrets
/// redacted.
fn describe(request: &Value) -> String {
    let url = request["url"].as_str().unwrap_or("?");
    let url = redact_url(url).map_or_else(|| url.to_string(), |(redacted, _)| redacted);
    format!("{} {url}", request["method"].as_str().unwrap_or("?"))
}

/// What in a HAR document (JSON, as a recording writes it) holds a secret
/// that should have been left out or redacted: credential headers in
/// requests, secret-looking form fields with real values in request
/// bodies, secret-looking parameters with real values in request URLs,
/// `Referer` and `Location`, response cookies, credential headers and
/// secret-looking fields of JSON responses whose values are not
/// placeholders, and keys not computed from the request as recorded
/// (which may come from a secret it held). Each item names the request
/// and the field, never the value.
pub fn unredacted_secrets(har: &[u8]) -> Result<Vec<String>, String> {
    let har: Value = serde_json::from_slice(har).map_err(|e| format!("parsing: {e}"))?;
    let entries = har["log"]["entries"]
        .as_array()
        .ok_or_else(|| "no entries".to_string())?;
    let mut found = Vec::new();
    for entry in entries {
        let request = &entry["request"];
        let what = describe(request);
        let url = request["url"].as_str().unwrap_or("");
        for field in redact_url(url).into_iter().flat_map(|(_, fields)| fields) {
            found.push(format!("{what}: URL parameter {field}"));
        }
        if let Some(key) = entry["_catpaw"]["key"].as_str()
            && recorded_key(request)
                .map_err(|e| format!("{what}: {e}"))?
                .is_some_and(|recorded| recorded != key)
        {
            found.push(format!(
                "{what}: a key not computed from the request as recorded"
            ));
        }
        let headers = |holder: &Value| -> Vec<(String, String)> {
            holder["headers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|h| {
                    (
                        h["name"].as_str().unwrap_or("").to_ascii_lowercase(),
                        h["value"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        };
        for (side, holder) in [("request", request), ("response", &entry["response"])] {
            for (name, value) in headers(holder) {
                if !is_url_header(&name) {
                    continue;
                }
                for field in redact_url(&value)
                    .into_iter()
                    .flat_map(|(_, fields)| fields)
                {
                    found.push(format!("{what}: {side} header {name}, parameter {field}"));
                }
            }
        }
        for (name, _) in headers(request) {
            if is_credential_header(&name) {
                found.push(format!("{what}: request header {name}"));
            }
        }
        if let Some(post) = request.get("postData") {
            let body = decode_text(post).map_err(|e| format!("{what}: {e}"))?;
            let mime = post["mimeType"].as_str().unwrap_or("");
            if let Some(redaction) = redact_body(mime, &body) {
                for field in redaction.fields {
                    found.push(format!("{what}: form field {field}"));
                }
            }
        }
        let content = &entry["response"]["content"];
        let mime = content["mimeType"].as_str().unwrap_or("");
        if let Ok(body) = decode_text(content) {
            if scrub_addresses(&body, mime).is_some() {
                found.push(format!("{what}: an address the response echoes"));
            }
            if is_json(mime) {
                let mut fields: Vec<String> = json_secrets(&body)
                    .into_iter()
                    .filter(|secret| !is_hidden(&secret.value))
                    .map(|secret| secret.name)
                    .collect();
                fields.sort();
                fields.dedup();
                for field in fields {
                    found.push(format!("{what}: response field {field}"));
                }
            }
        }
        for (name, value) in headers(&entry["response"]) {
            match name.as_str() {
                "set-cookie" | "set-cookie2" => {
                    let (cookie, value) = cookie_pair(&value);
                    if !value.is_empty() && !is_placeholder(value) {
                        found.push(format!("{what}: response cookie {cookie}"));
                    }
                }
                other
                    if is_credential_header(other)
                        && !value.is_empty()
                        && !is_placeholder(&value) =>
                {
                    found.push(format!("{what}: response header {other}"));
                }
                _ => {}
            }
        }
    }
    Ok(found)
}

/// A recording with each entry's key computed again from its request as
/// recorded, and how many keys that changed; `None` when none did.
/// Recordings made before keys were computed so hold keys computed from
/// the body as it was sent, secrets included. Nothing else changes, so a
/// recording not laid out as the recorder writes one is refused.
pub fn rekey(bytes: &[u8]) -> Result<Option<(Vec<u8>, usize)>, String> {
    let mut har: Value = serde_json::from_slice(bytes).map_err(|e| format!("parsing: {e}"))?;
    if serde_json::to_vec(&har).map_err(|e| format!("writing: {e}"))? != bytes {
        return Err("not laid out as a recording is written".to_string());
    }
    let entries = har["log"]["entries"]
        .as_array_mut()
        .ok_or_else(|| "no entries".to_string())?;
    let mut changed = 0;
    for entry in entries {
        let Some(key) = entry["_catpaw"]["key"].as_str() else {
            continue;
        };
        let request = &entry["request"];
        if let Some(recorded) =
            recorded_key(request).map_err(|e| format!("{}: {e}", describe(request)))?
            && recorded != key
        {
            entry["_catpaw"]["key"] = Value::String(recorded);
            changed += 1;
        }
    }
    if changed == 0 {
        return Ok(None);
    }
    let text = serde_json::to_vec(&har).map_err(|e| format!("writing: {e}"))?;
    Ok(Some((text, changed)))
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("catpaw-har-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    /// Records the first hop of the request at `place`.
    #[allow(clippy::too_many_arguments)]
    fn hop(
        recorder: &Recorder,
        place: Place,
        method: &Method,
        url: &str,
        request_headers: &HeaderMap,
        request_body: &[u8],
        response_headers: &HeaderMap,
        response_body: &[u8],
    ) {
        recorder.record(
            place,
            0,
            Exchange {
                method,
                url: &Url::parse(url).unwrap(),
                request_headers,
                request_body,
                status: StatusCode::OK,
                response_headers,
                response_body,
                started_ms: 1_000,
                took_ms: 5,
            },
        );
    }

    fn get(replayer: &Replayer, url: &str) -> Option<Bytes> {
        replayer
            .answer(&Method::GET, &Url::parse(url).unwrap(), "", b"")
            .map(|a| a.unwrap().body)
    }

    #[test]
    fn dates_are_iso() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_791_391_624_223), "2026-10-07T16:47:04.223Z");
    }

    #[test]
    fn recordings_round_trip_and_answer_in_order() {
        let dir = temp_dir("round-trip");
        let path = dir.join("t.har.zst");
        let recorder = Recorder::new(path.clone());
        let url = "https://shop.example/api?x=1#frag";
        let request_headers = headers(&[("cookie", "secret=1"), ("accept", "*/*")]);
        let response_headers = headers(&[("content-type", "text/plain")]);
        for body in ["first", "second"] {
            let place = recorder.reserve();
            hop(
                &recorder,
                place,
                &Method::GET,
                url,
                &request_headers,
                b"",
                &response_headers,
                body.as_bytes(),
            );
        }
        let place = recorder.reserve();
        hop(
            &recorder,
            place,
            &Method::GET,
            "https://shop.example/bin",
            &HeaderMap::new(),
            b"",
            &HeaderMap::new(),
            &[0xff, 0x00, 0x80],
        );
        assert_eq!(recorder.save("test").unwrap(), 3);
        let text = String::from_utf8(read_file(&path).unwrap()).unwrap();
        assert!(!text.contains("secret=1"), "cookies stay out");
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        assert_eq!(replayer.len(), 3);
        assert_eq!(
            get(&replayer, "https://shop.example/api?x=1").unwrap(),
            "first"
        );
        assert_eq!(
            get(&replayer, "https://shop.example/api?x=1#other").unwrap(),
            "second"
        );
        // Run out: the last answer repeats; another query: the loose key,
        // which has nothing left either.
        assert_eq!(
            get(&replayer, "https://shop.example/api?x=1").unwrap(),
            "second"
        );
        assert_eq!(
            get(&replayer, "https://shop.example/api?x=2").unwrap(),
            "second"
        );
        assert_eq!(
            get(&replayer, "https://shop.example/bin").unwrap().as_ref(),
            &[0xff, 0x00, 0x80]
        );
        assert!(get(&replayer, "https://shop.example/none").is_none());
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

    #[test]
    fn entries_keep_the_order_requests_were_made_in_hops_together() {
        let dir = temp_dir("order");
        let path = dir.join("t.har");
        let recorder = Recorder::new(path.clone());
        let url = "https://shop.example/same";
        let (a, b) = (recorder.reserve(), recorder.reserve());
        // The second request is answered first, and the first one's
        // redirect comes after that.
        for (place, number, body) in [(b, 0, "B"), (a, 1, "A after the redirect"), (a, 0, "A")] {
            recorder.record(
                place,
                number,
                Exchange {
                    method: &Method::GET,
                    url: &Url::parse(url).unwrap(),
                    request_headers: &HeaderMap::new(),
                    request_body: b"",
                    status: StatusCode::OK,
                    response_headers: &HeaderMap::new(),
                    response_body: body.as_bytes(),
                    started_ms: 1_000,
                    took_ms: 5,
                },
            );
        }
        recorder.save("test").unwrap();
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        let answers: Vec<Bytes> = (0..3).map(|_| get(&replayer, url).unwrap()).collect();
        assert_eq!(answers, ["A", "A after the redirect", "B"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn binary_request_bodies_are_kept_whole_and_match_exactly() {
        let dir = temp_dir("binary");
        let path = dir.join("t.har");
        let recorder = Recorder::new(path.clone());
        let url = "https://shop.example/upload";
        let bodies: [&[u8]; 2] = [&[0xff, 0xfe, 0x00, 0x01], &[0xff, 0xfe, 0x00, 0x02]];
        for (body, answer) in bodies.iter().zip(["one", "two"]) {
            let place = recorder.reserve();
            hop(
                &recorder,
                place,
                &Method::POST,
                url,
                &headers(&[("content-type", "application/octet-stream")]),
                body,
                &HeaderMap::new(),
                answer.as_bytes(),
            );
        }
        recorder.save("test").unwrap();
        let har: Value = serde_json::from_slice(&read_file(&path).unwrap()).unwrap();
        let post = &har["log"]["entries"][0]["request"]["postData"];
        assert_eq!(post["encoding"], "base64");
        assert_eq!(decode_text(post).unwrap(), bodies[0]);
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        let post = |body: &[u8]| {
            replayer
                .answer(
                    &Method::POST,
                    &Url::parse(url).unwrap(),
                    "application/octet-stream",
                    body,
                )
                .map(|a| a.unwrap().body)
        };
        // Asked the other way round: each body gets its own answer.
        assert_eq!(post(bodies[1]).unwrap(), "two");
        assert_eq!(post(bodies[0]).unwrap(), "one");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exact_and_path_matches_share_what_was_given() {
        let dir = temp_dir("shared");
        let path = dir.join("t.har");
        let recorder = Recorder::new(path.clone());
        for (t, body) in [("1", "A"), ("2", "B")] {
            let place = recorder.reserve();
            hop(
                &recorder,
                place,
                &Method::GET,
                &format!("https://shop.example/poll?t={t}"),
                &HeaderMap::new(),
                b"",
                &HeaderMap::new(),
                body.as_bytes(),
            );
        }
        recorder.save("test").unwrap();
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        let answers: Vec<Bytes> = ["1", "3", "4"]
            .iter()
            .map(|t| get(&replayer, &format!("https://shop.example/poll?t={t}")).unwrap())
            .collect();
        assert_eq!(answers, ["A", "B", "B"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn large_bodies_are_kept_and_old_truncated_ones_fail_clearly() {
        let dir = temp_dir("large");
        let path = dir.join("t.har");
        let recorder = Recorder::new(path.clone());
        let big = vec![b'x'; 5 * 1024 * 1024];
        let place = recorder.reserve();
        hop(
            &recorder,
            place,
            &Method::GET,
            "https://shop.example/big",
            &HeaderMap::new(),
            b"",
            &HeaderMap::new(),
            &big,
        );
        recorder.save("test").unwrap();
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        assert_eq!(
            get(&replayer, "https://shop.example/big").unwrap().len(),
            big.len()
        );

        let old = json!({"log": {"entries": [{
            "request": {"method": "GET", "url": "https://shop.example/big", "headers": []},
            "response": {"status": 200, "headers": [], "content": {
                "size": 5_242_880, "mimeType": "", "text": "", "_catpawTruncated": true}},
        }]}});
        let replayer = Replayer::from_bytes(old.to_string().as_bytes(), Misses::Fail).unwrap();
        let answer = replayer
            .answer(
                &Method::GET,
                &Url::parse("https://shop.example/big").unwrap(),
                "",
                b"",
            )
            .unwrap();
        let error = answer.unwrap_err();
        assert!(error.contains("left out the 5242880-byte body"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_path_match_keeps_the_port() {
        let har = json!({"log": {"entries": [{
            "request": {"method": "GET", "url": "http://shop.example:8080/api?x=1", "headers": []},
            "response": {"status": 200, "headers": [], "content": {"text": "8080"}},
        }]}});
        let replayer = Replayer::from_bytes(har.to_string().as_bytes(), Misses::Fail).unwrap();
        assert!(get(&replayer, "http://shop.example:9090/api?x=2").is_none());
        assert!(get(&replayer, "http://shop.example/api?x=2").is_none());
        assert_eq!(
            get(&replayer, "http://shop.example:8080/api?x=2").unwrap(),
            "8080"
        );
    }

    #[test]
    fn recordings_of_the_earlier_format_still_replay() {
        // As the recorder wrote them before: lossy post data, no
        // `encoding`, entries in the order responses completed, the key
        // from the body as sent.
        let url = Url::parse("https://shop.example/form").unwrap();
        let binary = [0xffu8, 0x00, b'a'];
        let har = json!({"log": {"version": "1.2", "entries": [
            {
                "request": {"method": "POST", "url": url.as_str(), "headers": [],
                    "postData": {"mimeType": "", "text": String::from_utf8_lossy(&binary)}},
                "response": {"status": 201, "headers": [
                    {"name": "set-cookie", "value": "s=1; Expires=Thu, 01 Jan 1970 00:10:00 GMT"}],
                    "content": {"text": "posted"}},
                "_catpaw": {"key": request_key("POST", &url, "", &binary), "startedMs": 0},
            },
            {
                "request": {"method": "GET", "url": "https://shop.example/img", "headers": []},
                "response": {"status": 200, "headers": [],
                    "content": {"text": base64_text(&[1, 2, 3]), "encoding": "base64"}},
            },
        ]}});
        let replayer = Replayer::from_bytes(har.to_string().as_bytes(), Misses::Fail).unwrap();
        let posted = replayer
            .answer(&Method::POST, &url, "", &binary)
            .unwrap()
            .unwrap();
        assert_eq!(posted.status, StatusCode::CREATED);
        assert_eq!(posted.body, "posted");
        let cookie = posted.headers.get(http::header::SET_COOKIE).unwrap();
        assert!(cookie.to_str().unwrap().contains("Max-Age=600"));
        assert_eq!(
            get(&replayer, "https://shop.example/img").unwrap().as_ref(),
            &[1, 2, 3]
        );
    }

    #[test]
    fn secret_fields_are_named_so() {
        for secret in [
            // Passwords.
            "password",
            "Passwd",
            "user[password]",
            "newPassword",
            "password_confirmation",
            "passphrase",
            "passcode",
            "pass",
            "user_pass",
            "pwd",
            "pw",
            "new_pw",
            "PW2",
            // Secrets, tokens and keys.
            "client_secret",
            "clientSecret",
            "csrf_token",
            "csrfmiddlewaretoken",
            "authenticity_token",
            "__RequestVerificationToken",
            "access_token",
            "refresh_token",
            "id_token",
            "apiKey",
            "api_key",
            "x-api-key",
            "apikey",
            "access_key",
            "secret_key",
            "private_key",
            "Ocp-Apim-Subscription-Key",
            "code_verifier",
            "Authorization",
            // One-time codes.
            "otp",
            "otp_code",
            "totp",
            "app_otp",
            "mfa",
            "mfa_code",
            "2fa",
            "2fa_code",
            "twoFactorCode",
            "one_time_code",
            "oneTimeCode",
            "one-time-code",
            "verification_code",
            "verificationCode",
            "verify_code",
            "auth_code",
            "authCode",
            "authorization_code",
            "sms_code",
            "security_code",
            "recovery_code",
            "backup_code",
            "login_code",
            "access_code",
            // PINs.
            "pin",
            "PIN",
            "card_pin",
            "pinCode",
            // A postcode in India too: taken for a PIN.
            "pincode",
            "userPIN",
            "pin1",
            "mpin",
            "pin_number",
            // Cards, social security numbers, sessions.
            "card",
            "cardNumber",
            "card[number]",
            "cardnumber",
            "creditcard",
            "cc_number",
            "ccnum",
            "cvv",
            "cvc2",
            "card_cvc",
            "ssn",
            "social_security_number",
            "session_id",
            "sessionId",
            "PHPSESSID",
            "JSESSIONID",
        ] {
            assert!(is_secret_field(secret), "{secret}");
        }
        for plain in [
            "username",
            "email",
            "q",
            "size",
            // Codes that are not secrets.
            "country_code",
            "countryCode",
            "zip_code",
            "zipCode",
            "zipcode",
            "postcode",
            "postal_code",
            "promo_code",
            "promoCode",
            "coupon_code",
            "couponCode",
            "discount_code",
            "voucher_code",
            "referral_code",
            "preset_code",
            "presetcode",
            "barcode",
            "status_code",
            "language_code",
            "currency_code",
            "area_code",
            "code",
            // A secret's name inside another word.
            "shipping",
            "spinner",
            "spin",
            "opinion",
            "passenger",
            "passport",
            "className",
            "classname",
            "rootPath",
            "footprint",
            "discard",
            "scorecard",
            "cards",
            "tokens",
            "prompt_tokens",
            "secretary",
            "monkey",
            // Names that describe a secret, or say something of it.
            "token_type",
            "token_endpoint",
            "expires_in",
            "pin_length",
            "password_hint",
            "password_policy",
            "authorization_endpoint",
            "access_key_id",
            "api_key_id",
            "card_brand",
            "card_last4",
            "card_id",
            "mfa_enabled",
            "session",
            "public_key",
            "primary_key",
        ] {
            assert!(!is_secret_field(plain), "{plain}");
        }
    }

    #[test]
    fn secret_form_fields_are_redacted_in_each_kind_of_body() {
        let form = redact_body(
            "application/x-www-form-urlencoded",
            b"user=tom&password=Super%21&remember=1&otp=",
        )
        .unwrap();
        assert_eq!(form.body, b"user=tom&password=redacted&remember=1&otp=");
        assert_eq!(form.fields, ["password"]);
        // Already redacted: nothing more to do.
        assert!(redact_body("application/x-www-form-urlencoded", &form.body).is_none());

        let plain = redact_body("text/plain", b"name=Ada\r\npin=1234\r\n").unwrap();
        assert_eq!(plain.body, b"name=Ada\r\npin=redacted\r\n");

        let json = redact_body(
            "application/json",
            br#"{"user":"ada","auth":{"token":"abc","ttl":3},"card":{"number":4111,"name":"A"}}"#,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&json.body).unwrap();
        assert_eq!(value["user"], "ada");
        assert_eq!(value["auth"]["token"], "redacted");
        assert_eq!(value["auth"]["ttl"], 3);
        assert_eq!(value["card"]["number"], "redacted");
        assert_eq!(value["card"]["name"], "redacted");
        // fetch() sends strings as text/plain.
        assert!(redact_body("text/plain;charset=UTF-8", br#"{"password":"x"}"#).is_some());

        let multipart = b"--XyZ\r\nContent-Disposition: form-data; name=\"user\"\r\n\r\nada\r\n--XyZ\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\nhunter2\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--XyZ--\r\n";
        let redacted = redact_body("multipart/form-data; boundary=XyZ", multipart).unwrap();
        let text = String::from_utf8(redacted.body).unwrap();
        assert_eq!(
            text,
            String::from_utf8_lossy(multipart).replace("hunter2", "redacted")
        );
        assert_eq!(redacted.fields, ["password"]);

        assert!(redact_body("", b"user=tom&pass=x").is_some());
        assert!(redact_body("image/png", b"pass=x").is_none());
    }

    #[test]
    fn recordings_leave_secrets_out_and_still_match_the_real_body() {
        let dir = temp_dir("secrets");
        let path = dir.join("t.har");
        let recorder = Recorder::new(path.clone());
        let url = "https://shop.example/login";
        let form = "application/x-www-form-urlencoded";
        let body = b"username=tomsmith&password=SuperSecretPassword%21";
        let place = recorder.reserve();
        hop(
            &recorder,
            place,
            &Method::POST,
            url,
            &headers(&[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", "rack.session=abc"),
                ("x-csrf-token", "tok123"),
            ]),
            body,
            &headers(&[
                (
                    "set-cookie",
                    "rack.session=BAh7CUkiD3Nlc3Npb25faWQ; path=/; HttpOnly",
                ),
                ("set-cookie", "theme=dark; Path=/"),
                ("set-cookie", "gone=; Max-Age=0"),
                ("x-auth-token", "tok456"),
                ("content-type", "text/html"),
            ]),
            b"<p>welcome</p>",
        );
        // Another user's login to the same address, recorded after.
        let place = recorder.reserve();
        hop(
            &recorder,
            place,
            &Method::POST,
            url,
            &headers(&[("content-type", "application/x-www-form-urlencoded")]),
            b"username=ada&password=Engine1843",
            &HeaderMap::new(),
            b"<p>hello Ada</p>",
        );
        recorder.save("test").unwrap();
        let bytes = read_file(&path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for secret in [
            "SuperSecret",
            "Engine1843",
            "tok123",
            "tok456",
            "BAh7CUkiD3Nlc3Npb25faWQ",
            "dark",
            "rack.session=abc",
        ] {
            assert!(!text.contains(secret), "{secret} is in {text}");
        }
        assert!(text.contains("password=redacted"), "{text}");
        assert!(
            text.contains("; path=/; HttpOnly"),
            "attributes stay: {text}"
        );
        assert_eq!(unredacted_secrets(&bytes).unwrap(), Vec::<String>::new());

        // The key comes from the body as recorded: it is the same whatever
        // the password was, and tells nothing of it.
        let har: Value = serde_json::from_slice(&bytes).unwrap();
        let key = har["log"]["entries"][0]["_catpaw"]["key"].as_str().unwrap();
        let login = Url::parse(url).unwrap();
        for password in ["SuperSecretPassword%21", "guess", "redacted"] {
            let body = format!("username=tomsmith&password={password}");
            assert_eq!(
                request_key("POST", &login, form, body.as_bytes()),
                key,
                "{password}"
            );
        }
        assert_eq!(
            key,
            format!(
                "POST {url} {:016x}",
                fnv(b"username=tomsmith&password=redacted")
            )
        );

        // A replayed login, redacted the same way, matches its own entry:
        // asked first, the later login gets its answer.
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        let ada = replayer
            .answer(
                &Method::POST,
                &login,
                form,
                b"username=ada&password=Engine1843",
            )
            .unwrap()
            .unwrap();
        assert_eq!(ada.body, "<p>hello Ada</p>");
        let answer = replayer
            .answer(&Method::POST, &login, form, body)
            .unwrap()
            .unwrap();
        assert_eq!(answer.body, "<p>welcome</p>");
        let cookies: Vec<&str> = answer
            .headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(cookies.len(), 3);
        let (name, value) = cookie_pair(cookies[0]);
        assert_eq!(name, "rack.session");
        assert!(is_placeholder(value), "{value}");
        assert_eq!(cookies[2], "gone=; Max-Age=0");
        // Equal values, equal placeholders.
        assert_eq!(
            recorder.redact_set_cookie("a=v1; Path=/"),
            recorder.redact_set_cookie("a=v1; Path=/")
        );
        assert_ne!(
            recorder.redact_set_cookie("a=v1"),
            recorder.redact_set_cookie("a=v2")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn echoed_addresses_are_kept_out() {
        let body = br#"{"origin": "198.51.100.23, 10.0.0.1", "url": "https://httpbin.org/post", "nested": {"ip": "2001:db8::1"}}"#;
        let scrubbed = scrub_addresses(body, "application/json").unwrap();
        let text = String::from_utf8(scrubbed).unwrap();
        assert!(!text.contains("198.51.100.23"), "{text}");
        assert!(!text.contains("10.0.0.1"), "{text}");
        assert!(!text.contains("2001:db8::1"), "{text}");
        assert!(text.contains(STAND_IN_ADDRESS), "{text}");
        assert!(text.contains("https://httpbin.org/post"), "{text}");
        assert!(scrub_addresses(body, "text/html").is_none());
        assert!(
            scrub_addresses(br#"{"origin": "https://a.example"}"#, "application/json").is_none()
        );
        let har = serde_json::json!({"log": {"entries": [{
            "request": {"method": "POST", "url": "https://httpbin.org/post", "headers": []},
            "response": {"status": 200, "headers": [], "content": {
                "mimeType": "application/json", "text": String::from_utf8_lossy(body)}}
        }]}});
        let found = unredacted_secrets(har.to_string().as_bytes()).unwrap();
        assert_eq!(
            found,
            ["POST https://httpbin.org/post: an address the response echoes"]
        );
    }

    #[test]
    fn unredacted_secrets_are_found() {
        let har = json!({"log": {"entries": [{
            "request": {"method": "POST",
                "url": "https://shop.example/login?next=%2F&api_key=k1#access_token=t1",
                "headers": [
                    {"name": "Authorization", "value": "Bearer x"},
                    {"name": "Referer", "value": "https://shop.example/start?session_id=s1"}],
                "postData": {"mimeType": "application/x-www-form-urlencoded",
                    "text": "user=a&password=hunter2"}},
            "response": {"status": 302, "headers": [
                {"name": "Set-Cookie", "value": "sid=abc123; HttpOnly"},
                {"name": "Set-Cookie", "value": "ok=redacted-0a1b2c3d"},
                {"name": "X-CSRF-Token", "value": "t0k3n"},
                {"name": "Location", "value": "/home?verification_code=123456"}],
                "content": {"mimeType": "application/json", "text":
                    r#"{"refresh_token": "r1", "token_type": "Bearer", "client_secret": "redacted-0a1b2c3d"}"#}},
            "_catpaw": {"key": "POST https://shop.example/login?next=%2F&api_key=k1 0123456789abcdef"},
        }]}});
        let found = unredacted_secrets(har.to_string().as_bytes()).unwrap();
        let what =
            "POST https://shop.example/login?next=%2F&api_key=redacted#access_token=redacted";
        let expected: Vec<String> = [
            "URL parameter api_key",
            "URL parameter access_token",
            "a key not computed from the request as recorded",
            "request header referer, parameter session_id",
            "response header location, parameter verification_code",
            "request header authorization",
            "form field password",
            "response field refresh_token",
            "response cookie sid",
            "response header x-csrf-token",
        ]
        .iter()
        .map(|item| format!("{what}: {item}"))
        .collect();
        assert_eq!(found, expected);
        for value in [
            "k1", "t1", "s1", "123456", "hunter2", "r1", "abc123", "t0k3n",
        ] {
            assert!(
                found.iter().all(|item| !item.contains(value)),
                "{value}: {found:?}"
            );
        }
    }

    #[test]
    fn secret_url_parameters_are_redacted_and_requests_still_match() {
        assert_eq!(redact_url("/a?b=1&code=US"), None);
        assert_eq!(redact_url("/a#section"), None);
        assert_eq!(redact_url("/a?token=redacted&pin="), None);
        assert_eq!(
            redact_url("/a?api%5Fkey=k&x=%20#/route?token=t"),
            Some((
                "/a?api%5Fkey=redacted&x=%20#/route?token=redacted".to_string(),
                vec!["api_key".to_string(), "/route?token".to_string()]
            ))
        );

        let dir = temp_dir("url-secrets");
        let path = dir.join("t.har");
        let recorder = Recorder::new(path.clone());
        for (page, token, answer) in [("1", "tokA", "first"), ("2", "tokB", "second")] {
            let place = recorder.reserve();
            hop(
                &recorder,
                place,
                &Method::GET,
                &format!(
                    "https://shop.example/api?page={page}&access_token={token}#id_token=idt{page}&state=s"
                ),
                &headers(&[(
                    "referer",
                    "https://shop.example/callback?code=1&auth_code=AC1",
                )]),
                b"",
                &headers(&[("location", "/next?x=1&session_id=SID1")]),
                answer.as_bytes(),
            );
        }
        recorder.save("test").unwrap();
        let bytes = read_file(&path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for secret in ["tokA", "tokB", "idt1", "idt2", "AC1", "SID1"] {
            assert!(!text.contains(secret), "{secret} is in {text}");
        }
        let har: Value = serde_json::from_slice(&bytes).unwrap();
        let entry = &har["log"]["entries"][0];
        assert_eq!(
            entry["request"]["url"],
            "https://shop.example/api?page=1&access_token=redacted#id_token=redacted&state=s"
        );
        assert_eq!(
            entry["request"]["headers"][0]["value"],
            "https://shop.example/callback?code=1&auth_code=redacted"
        );
        assert_eq!(
            entry["response"]["headers"][0]["value"],
            "/next?x=1&session_id=redacted"
        );
        assert_eq!(
            entry["response"]["redirectURL"],
            "/next?x=1&session_id=redacted"
        );
        assert_eq!(unredacted_secrets(&bytes).unwrap(), Vec::<String>::new());

        // A key holds the URL as recorded, whatever the token was.
        let url = |query: &str| Url::parse(&format!("https://shop.example/api?{query}")).unwrap();
        assert_eq!(
            request_key("GET", &url("page=1&access_token=tokA"), "", b""),
            request_key("GET", &url("page=1&access_token=other"), "", b"")
        );
        assert_eq!(
            entry["_catpaw"]["key"],
            format!(
                "GET {} {:016x}",
                url("page=1&access_token=redacted"),
                fnv(b"")
            )
        );
        // Asked first, the second page gets its own answer.
        let replayer = Replayer::open(&path, Misses::Fail).unwrap();
        assert_eq!(
            get(
                &replayer,
                "https://shop.example/api?page=2&access_token=tokB"
            )
            .unwrap(),
            "second"
        );
        assert_eq!(
            get(
                &replayer,
                "https://shop.example/api?page=1&access_token=tokA"
            )
            .unwrap(),
            "first"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secret_fields_of_json_responses_become_placeholders() {
        let dir = temp_dir("json-secrets");
        let path = dir.join("t.har");
        let recorder = Recorder::new(path.clone());
        let body = br#"{ "token_type": "Bearer", "access_token": "tok\"A", "expires_in": 3600,
  "user": {"name": "Ada", "pin": 1234, "sessions": [{"refresh_token": "tokB"}]},
  "copy": {"id_token": "tok\"A"}, "client_secret": "", "secrets": ["s1", {"k": "s2"}] }"#;
        let redacted = recorder.redact_json_response(body).unwrap();
        let p = |value: &str| recorder.placeholder(value);
        // Each value under a secret name, at any depth, and nothing else.
        let expected = format!(
            r#"{{ "token_type": "Bearer", "access_token": "{a}", "expires_in": 3600,
  "user": {{"name": "Ada", "pin": "{pin}", "sessions": [{{"refresh_token": "{b}"}}]}},
  "copy": {{"id_token": "{a}"}}, "client_secret": "", "secrets": ["{s1}", {{"k": "{s2}"}}] }}"#,
            a = p("tok\"A"),
            pin = p("1234"),
            b = p("tokB"),
            s1 = p("s1"),
            s2 = p("s2"),
        );
        assert_eq!(String::from_utf8(redacted.clone()).unwrap(), expected);
        assert_ne!(p("tokB"), p("s1"));
        assert!(recorder.redact_json_response(&redacted).is_none());
        assert!(
            recorder
                .redact_json_response(b"<p>access_token</p>")
                .is_none()
        );

        let json = headers(&[("content-type", "application/json")]);
        for (place, mime) in [
            (recorder.reserve(), &json),
            (recorder.reserve(), &HeaderMap::new()),
        ] {
            hop(
                &recorder,
                place,
                &Method::POST,
                "https://shop.example/oauth/token",
                &HeaderMap::new(),
                b"",
                mime,
                br#"{"access_token": "tokC"}"#,
            );
        }
        recorder.save("test").unwrap();
        let bytes = read_file(&path).unwrap();
        let har: Value = serde_json::from_slice(&bytes).unwrap();
        let content = |i: usize| har["log"]["entries"][i]["response"]["content"]["text"].clone();
        assert_eq!(
            content(0),
            format!(r#"{{"access_token": "{}"}}"#, p("tokC"))
        );
        // Not said to be JSON: kept as it is.
        assert_eq!(content(1), r#"{"access_token": "tokC"}"#);
        assert_eq!(unredacted_secrets(&bytes).unwrap(), Vec::<String>::new());

        let leftover = json!({"log": {"entries": [{
            "request": {"method": "GET", "url": "https://shop.example/me", "headers": []},
            "response": {"status": 200, "headers": [], "content": {
                "mimeType": "application/json; charset=utf-8",
                "text": String::from_utf8_lossy(body)}},
        }]}});
        assert_eq!(
            unredacted_secrets(leftover.to_string().as_bytes()).unwrap(),
            [
                "GET https://shop.example/me: response field access_token",
                "GET https://shop.example/me: response field id_token",
                "GET https://shop.example/me: response field pin",
                "GET https://shop.example/me: response field refresh_token",
                "GET https://shop.example/me: response field secrets",
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys_are_computed_again_from_the_requests_as_recorded() {
        // A recording made when keys came from the body as sent.
        let url = Url::parse("https://shop.example/login").unwrap();
        let form = "application/x-www-form-urlencoded";
        let sent = b"username=tomsmith&password=SuperSecretPassword%21";
        let old_key = format!("POST {url} {:016x}", fnv(sent));
        let har = json!({"log": {"entries": [
            {
                "request": {"method": "POST", "url": url.as_str(), "headers": [],
                    "postData": {"mimeType": form, "text": "username=tomsmith&password=redacted"}},
                "response": {"status": 302, "headers": [], "content": {"text": ""}},
                "_catpaw": {"key": old_key, "startedMs": 0},
            },
            {
                "request": {"method": "GET", "url": "https://shop.example/secure", "headers": []},
                "response": {"status": 200, "headers": [], "content": {"text": "in"}},
                "_catpaw": {"key": "GET https://shop.example/secure cbf29ce484222325", "startedMs": 0},
            },
        ]}});
        let bytes = serde_json::to_vec(&har).unwrap();
        assert_eq!(
            unredacted_secrets(&bytes).unwrap(),
            ["POST https://shop.example/login: a key not computed from the request as recorded"]
        );
        let (rekeyed, changed) = rekey(&bytes).unwrap().unwrap();
        assert_eq!(changed, 1);
        // Only the key changes.
        let new_key = request_key("POST", &url, form, sent);
        assert_eq!(
            String::from_utf8(rekeyed.clone()).unwrap(),
            String::from_utf8(bytes)
                .unwrap()
                .replace(&old_key, &new_key)
        );
        assert_eq!(unredacted_secrets(&rekeyed).unwrap(), Vec::<String>::new());
        assert!(rekey(&rekeyed).unwrap().is_none());
        let replayer = Replayer::from_bytes(&rekeyed, Misses::Fail).unwrap();
        let answer = replayer
            .answer(&Method::POST, &url, form, sent)
            .unwrap()
            .unwrap();
        assert_eq!(answer.status, StatusCode::FOUND);
        // Laid out otherwise, a recording is left alone.
        assert!(rekey(serde_json::to_string_pretty(&har).unwrap().as_bytes()).is_err());
    }
}
