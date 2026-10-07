//! A per-client cookie jar on top of `cookie_store` (RFC 6265bis semantics:
//! domain/path matching, Secure, HttpOnly, SameSite, expiry).

use std::sync::Mutex;

use cookie_store::CookieStore;
use http::HeaderMap;
use http::header::SET_COOKIE;
use url::Url;

#[derive(Default)]
pub struct CookieJar {
    store: Mutex<CookieStore>,
}

impl std::fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieJar")
            .field("cookies", &self.len())
            .finish()
    }
}

/// A cookie as seen by the agent layer (no secrets beyond what the page
/// itself could read, except `http_only` ones which the jar still lists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieEntry {
    pub domain: String,
    pub path: String,
    pub name: String,
    pub value: String,
    pub secure: bool,
    pub http_only: bool,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `Cookie` request header value for `url`, if any cookie matches.
    pub fn request_header(&self, url: &Url) -> Option<String> {
        let store = self.store.lock().expect("cookie jar poisoned");
        let pairs: Vec<String> = store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    /// Stores every `Set-Cookie` header of a response received from `url`.
    pub fn store_response(&self, url: &Url, headers: &HeaderMap) {
        let cookies = headers
            .get_all(SET_COOKIE)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .filter_map(|s| cookie::Cookie::parse(s).ok());
        self.store
            .lock()
            .expect("cookie jar poisoned")
            .store_response_cookies(cookies, url);
    }

    /// The cookies script may read for `url` (`document.cookie`): the
    /// matching cookies that are not `HttpOnly`, as a `Cookie` header value.
    pub fn script_header(&self, url: &Url) -> String {
        let store = self.store.lock().expect("cookie jar poisoned");
        store
            .matches(url)
            .into_iter()
            .filter(|c| !c.http_only().unwrap_or(false))
            .map(|c| format!("{}={}", c.name(), c.value()))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Stores a cookie set by script (`document.cookie = "..."`). Script
    /// cannot create `HttpOnly` cookies; such a string is ignored.
    pub fn store_from_script(&self, url: &Url, cookie: &str) {
        let Ok(parsed) = cookie::Cookie::parse(cookie.to_string()) else {
            return;
        };
        if parsed.http_only().unwrap_or(false) {
            return;
        }
        let _ = self
            .store
            .lock()
            .expect("cookie jar poisoned")
            .insert_raw(&parsed, url);
    }

    pub fn len(&self) -> usize {
        self.store
            .lock()
            .expect("cookie jar poisoned")
            .iter_unexpired()
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All unexpired cookies, for inspection and checkpoints.
    /// The jar as JSON (one cookie per line, as `cookie_store` writes it),
    /// for keeping between runs. Session cookies are included.
    pub fn to_json(&self) -> String {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        let _ = cookie_store::serde::json::save_incl_expired_and_nonpersistent(&store, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Adds the cookies of a jar saved with [`CookieJar::to_json`].
    pub fn load_json(&self, json: &str) -> Result<usize, String> {
        let loaded = cookie_store::serde::json::load_all(json.as_bytes())
            .map_err(|e| format!("reading the cookie file: {e}"))?;
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0;
        for cookie in loaded.iter_any() {
            let _ = store.insert_raw(cookie, &cookie_url(cookie));
            count += 1;
        }
        Ok(count)
    }

    pub fn entries(&self) -> Vec<CookieEntry> {
        self.store
            .lock()
            .expect("cookie jar poisoned")
            .iter_unexpired()
            .map(|c| CookieEntry {
                domain: c.domain().unwrap_or_default().to_string(),
                path: c.path().unwrap_or("/").to_string(),
                name: c.name().to_string(),
                value: c.value().to_string(),
                secure: c.secure().unwrap_or(false),
                http_only: c.http_only().unwrap_or(false),
            })
            .collect()
    }
}

/// A URL the cookie would have been set from, for re-inserting it.
fn cookie_url(cookie: &cookie_store::Cookie<'_>) -> Url {
    let domain = match &cookie.domain {
        cookie_store::CookieDomain::HostOnly(host) | cookie_store::CookieDomain::Suffix(host) => {
            host.as_str()
        }
        _ => "localhost",
    };
    let scheme = if cookie.secure().unwrap_or(false) {
        "https"
    } else {
        "http"
    };
    let path = cookie.path().unwrap_or("/");
    Url::parse(&format!("{scheme}://{domain}{path}"))
        .unwrap_or_else(|_| Url::parse("http://localhost/").expect("a valid URL"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn stores_and_returns_matching_cookies() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/a/b").unwrap();
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_static("a=1; Path=/"));
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("b=2; Path=/other; Secure"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("c=3; Domain=example.com; HttpOnly"),
        );
        jar.store_response(&url, &headers);
        assert_eq!(jar.len(), 3);
        let header = jar.request_header(&url).unwrap();
        assert!(header.contains("a=1"), "{header}");
        assert!(header.contains("c=3"), "{header}");
        assert!(
            !header.contains("b=2"),
            "path mismatch must exclude b: {header}"
        );
        assert_eq!(
            jar.request_header(&Url::parse("https://other.example/").unwrap()),
            None
        );
    }

    #[test]
    fn script_access_excludes_http_only_cookies() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/").unwrap();
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_static("seen=1"));
        headers.append(SET_COOKIE, HeaderValue::from_static("secret=2; HttpOnly"));
        jar.store_response(&url, &headers);
        assert_eq!(jar.script_header(&url), "seen=1");

        jar.store_from_script(&url, "mine=3; Path=/");
        jar.store_from_script(&url, "forged=4; HttpOnly");
        let visible = jar.script_header(&url);
        assert!(
            visible.contains("mine=3") && !visible.contains("forged"),
            "{visible}"
        );
        // Script-set cookies are sent with requests like any other.
        assert!(jar.request_header(&url).unwrap().contains("mine=3"));
    }
}
