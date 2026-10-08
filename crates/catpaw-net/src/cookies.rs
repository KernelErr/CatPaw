//! A per-client cookie jar on top of `cookie_store` (RFC 6265bis semantics:
//! domain/path matching, Secure, HttpOnly, SameSite, expiry).
//!
//! Cookies are listed in the order RFC 6265 §5.4 gives (longer paths
//! first, then those created earlier), in `Cookie` headers,
//! `document.cookie` and saved jars alike, so that a run sends and shows
//! them the same way every time. A cookie set again keeps the creation
//! time of the one it replaces; one set after it was deleted or expired is
//! new.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use cookie_store::{CookieStore, RawCookie, StoreAction};
use http::HeaderMap;
use http::header::SET_COOKIE;
use url::Url;

type StoredCookie = cookie_store::Cookie<'static>;

#[derive(Default)]
pub struct CookieJar {
    jar: Mutex<Jar>,
}

/// A cookie's identity in the store: domain, path and name.
type CookieKey = (String, String, String);

fn key_of(cookie: &StoredCookie) -> CookieKey {
    (
        String::from(&cookie.domain),
        String::from(&cookie.path),
        cookie.name().to_string(),
    )
}

#[derive(Default)]
struct Jar {
    store: CookieStore,
    /// When each cookie was created, as a count.
    created: HashMap<CookieKey, u64>,
    next: u64,
}

impl Jar {
    /// Stores a cookie received from (or set by script on) `url`, as
    /// `CookieStore::insert_raw` does, noting when it was created.
    fn insert(&mut self, raw: &RawCookie<'_>, url: &Url) {
        let Ok(cookie) = cookie_store::Cookie::try_from_raw_cookie(raw, url) else {
            return;
        };
        let cookie = cookie.into_owned();
        let key = key_of(&cookie);
        let replaces_live = self.store.get(&key.0, &key.1, &key.2).is_some();
        if let Ok(StoreAction::Inserted | StoreAction::UpdatedExisting) =
            self.store.insert(cookie, url)
            && !(replaces_live && self.created.contains_key(&key))
        {
            self.created.insert(key, self.next);
            self.next += 1;
        }
    }

    fn created(&self, cookie: &StoredCookie) -> u64 {
        self.created
            .get(&key_of(cookie))
            .copied()
            .unwrap_or(u64::MAX)
    }

    /// `cookies` in RFC 6265 order: longer paths first, then earlier
    /// creation (and, for cookies the jar knows no creation of, by
    /// domain and name).
    fn sorted<'a>(&self, mut cookies: Vec<&'a StoredCookie>) -> Vec<&'a StoredCookie> {
        cookies.sort_by_cached_key(|c| {
            let (domain, path, name) = key_of(c);
            (Reverse(path.len()), self.created(c), domain, name)
        });
        cookies
    }

    /// Every cookie, expired ones too, in the order they were created.
    fn by_creation(&self) -> Vec<&StoredCookie> {
        let mut cookies: Vec<&StoredCookie> = self.store.iter_any().collect();
        cookies.sort_by_cached_key(|c| (self.created(c), key_of(c)));
        cookies
    }
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

    fn lock(&self) -> MutexGuard<'_, Jar> {
        self.jar.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The `Cookie` request header value for `url`, if any cookie matches.
    pub fn request_header(&self, url: &Url) -> Option<String> {
        let jar = self.lock();
        let pairs: Vec<String> = jar
            .sorted(jar.store.matches(url))
            .into_iter()
            .map(|c| format!("{}={}", c.name(), c.value()))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    /// Stores every `Set-Cookie` header of a response received from `url`.
    pub fn store_response(&self, url: &Url, headers: &HeaderMap) {
        let mut jar = self.lock();
        for value in headers.get_all(SET_COOKIE) {
            let text = String::from_utf8_lossy(value.as_bytes()).into_owned();
            if let Ok(cookie) = RawCookie::parse(text) {
                jar.insert(&cookie, url);
            }
        }
    }

    /// The cookies script may read for `url` (`document.cookie`): the
    /// matching cookies that are not `HttpOnly`, as a `Cookie` header value.
    pub fn script_header(&self, url: &Url) -> String {
        let jar = self.lock();
        let visible = jar
            .store
            .matches(url)
            .into_iter()
            .filter(|c| !c.http_only().unwrap_or(false))
            .collect();
        jar.sorted(visible)
            .into_iter()
            .map(|c| format!("{}={}", c.name(), c.value()))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Stores a cookie set by script (`document.cookie = "..."`). Script
    /// cannot create `HttpOnly` cookies; such a string is ignored.
    pub fn store_from_script(&self, url: &Url, cookie: &str) {
        let Ok(parsed) = RawCookie::parse(cookie.to_string()) else {
            return;
        };
        if parsed.http_only().unwrap_or(false) {
            return;
        }
        self.lock().insert(&parsed, url);
    }

    pub fn len(&self) -> usize {
        self.lock().store.iter_unexpired().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets every cookie (a checkpoint being restored, say).
    pub fn clear(&self) {
        let mut jar = self.lock();
        jar.store.clear();
        jar.created.clear();
    }

    /// The jar as JSON (the array of cookies `cookie_store` writes, oldest
    /// first), for keeping between runs. Session cookies are included.
    pub fn to_json(&self) -> String {
        let jar = self.lock();
        match serde_json::to_string_pretty(&jar.by_creation()) {
            Ok(text) => format!("{text}\n"),
            Err(_) => "[]\n".to_string(),
        }
    }

    /// Adds the cookies of a jar saved with [`CookieJar::to_json`], as if
    /// created in the order they are listed.
    pub fn load_json(&self, json: &str) -> Result<usize, String> {
        let loaded: Vec<StoredCookie> =
            serde_json::from_str(json).map_err(|e| format!("reading the cookie file: {e}"))?;
        let mut jar = self.lock();
        for cookie in &loaded {
            jar.insert(cookie, &cookie_url(cookie));
        }
        Ok(loaded.len())
    }

    /// All unexpired cookies, oldest first, for inspection and checkpoints.
    pub fn entries(&self) -> Vec<CookieEntry> {
        let jar = self.lock();
        jar.by_creation()
            .into_iter()
            .filter(|c| !c.is_expired())
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

    fn set(jar: &CookieJar, url: &Url, cookies: &[&'static str]) {
        let mut headers = HeaderMap::new();
        for cookie in cookies {
            headers.append(SET_COOKIE, HeaderValue::from_static(cookie));
        }
        jar.store_response(url, &headers);
    }

    #[test]
    fn stores_and_returns_matching_cookies() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/a/b").unwrap();
        set(
            &jar,
            &url,
            &[
                "a=1; Path=/",
                "b=2; Path=/other; Secure",
                "c=3; Domain=example.com; HttpOnly",
            ],
        );
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
        set(&jar, &url, &["seen=1", "secret=2; HttpOnly"]);
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

    #[test]
    fn cookies_come_in_rfc_order_every_time() {
        let url = Url::parse("https://example.com/a/b/c").unwrap();
        for _ in 0..8 {
            let jar = CookieJar::new();
            let names = ["k", "e", "y", "s", "o", "r", "d", "x"];
            for name in names {
                jar.store_from_script(&url, &format!("{name}=1; Path=/"));
            }
            set(&jar, &url, &["deep=1; Path=/a/b", "deeper=1; Path=/a/b/c"]);
            jar.store_from_script(&Url::parse("https://sub.example.com/").unwrap(), "z=1");
            set(&jar, &url, &["wide=1; Domain=example.com; Path=/"]);
            let expected = "deeper=1; deep=1; k=1; e=1; y=1; s=1; o=1; r=1; d=1; x=1; wide=1";
            assert_eq!(jar.request_header(&url).unwrap(), expected);
            assert_eq!(jar.script_header(&url), expected);

            // Set again: it keeps its place. Deleted and set anew: it is
            // the newest.
            jar.store_from_script(&url, "e=2; Path=/");
            set(&jar, &url, &["k=; Max-Age=0; Path=/"]);
            jar.store_from_script(&url, "k=3; Path=/");
            assert_eq!(
                jar.script_header(&url),
                "deeper=1; deep=1; e=2; y=1; s=1; o=1; r=1; d=1; x=1; wide=1; k=3"
            );

            // A saved jar comes back in the same order.
            let copy = CookieJar::new();
            copy.load_json(&jar.to_json()).unwrap();
            assert_eq!(copy.script_header(&url), jar.script_header(&url));
            let names: Vec<String> = copy.entries().into_iter().map(|c| c.name).collect();
            assert_eq!(
                names,
                [
                    "e", "y", "s", "o", "r", "d", "x", "deep", "deeper", "z", "wide", "k"
                ]
            );
        }
    }
}
