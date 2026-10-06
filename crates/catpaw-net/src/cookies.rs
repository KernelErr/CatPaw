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
}
