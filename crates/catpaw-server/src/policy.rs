//! What needs the user's approval before it happens, and what may not
//! happen at all (ADR 0006, decision 8).
//!
//! Presets: `default` asks before a navigation that sends data (a form
//! posted, by the user's click or by script) and before files are
//! uploaded; script's own requests go, and show as consequences.
//! `strict` also asks before script sends data to another site and before
//! `evaluate`. `open` asks for nothing, for test runs. Trusted hosts need
//! no approval for what is sent to them; uploads ask whatever the host,
//! since the files are the user's. Allowed domains, when set, are the only
//! ones whose documents a tab may show: its page, the popups it opens and
//! the frames within them, every redirect hop included. They do not limit
//! the requests a page makes for its scripts, styles, images and data.

use url::Url;

/// A named set of rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preset {
    #[default]
    Default,
    Strict,
    Open,
}

impl Preset {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "default" => Some(Preset::Default),
            "strict" => Some(Preset::Strict),
            "open" => Some(Preset::Open),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Default => "default",
            Preset::Strict => "strict",
            Preset::Open => "open",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub preset: Preset,
    /// Hosts (with their subdomains) whose navigations and requests need
    /// no approval.
    pub trusted: Vec<String>,
    /// When not empty, the only domains (with their subdomains) whose
    /// documents a tab may show, frames included.
    pub allowed_domains: Vec<String>,
}

/// What a policy says about something about to happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Only once the user approves.
    Confirm,
    /// Never; the reason says why.
    Block(String),
}

/// Whether `url`'s host is one of `domains` or under one.
fn covers(domains: &[String], url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    domains.iter().any(|domain| {
        let domain = domain.trim().trim_start_matches('.').to_ascii_lowercase();
        !domain.is_empty()
            && (host == domain
                || host
                    .strip_suffix(&domain)
                    .is_some_and(|rest| rest.ends_with('.')))
    })
}

fn sends_data(method: &str) -> bool {
    !matches!(method, "GET" | "HEAD" | "OPTIONS")
}

impl Policy {
    /// A navigation about to load `url` with `method`, of a tab or of a
    /// frame within it.
    pub fn navigation(&self, method: &str, url: &Url) -> Verdict {
        let web = matches!(url.scheme(), "http" | "https");
        if web && !self.allowed_domains.is_empty() && !covers(&self.allowed_domains, url) {
            let host = url.host_str().unwrap_or_default();
            return Verdict::Block(format!("{host} is not an allowed domain"));
        }
        if self.preset == Preset::Open || covers(&self.trusted, url) {
            return Verdict::Allow;
        }
        if sends_data(method) {
            Verdict::Confirm
        } else {
            Verdict::Allow
        }
    }

    /// A request script makes to `url`, from a page of `origin` (its
    /// `Origin` header, when it sent one). A WebSocket sends data however
    /// it opens: whatever the page writes to it, once it is open.
    pub fn request(&self, method: &str, url: &Url, origin: Option<&str>) -> Verdict {
        let sends = sends_data(method) || matches!(url.scheme(), "ws" | "wss");
        if self.preset != Preset::Strict || !sends || covers(&self.trusted, url) {
            return Verdict::Allow;
        }
        let same_site = origin
            .and_then(|o| Url::parse(o).ok())
            .is_some_and(|from| catpaw_web::settle::same_site(&from, url));
        if same_site {
            Verdict::Allow
        } else {
            Verdict::Confirm
        }
    }

    /// Choosing local files in a file input (they go to the site later).
    pub fn upload(&self) -> Verdict {
        match self.preset {
            Preset::Open => Verdict::Allow,
            _ => Verdict::Confirm,
        }
    }

    /// Running the agent's script in the page.
    pub fn evaluate(&self) -> Verdict {
        match self.preset {
            Preset::Strict => Verdict::Confirm,
            _ => Verdict::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn posting_asks_and_reading_does_not() {
        let policy = Policy::default();
        let post = url("https://shop.example/checkout");
        assert_eq!(policy.navigation("POST", &post), Verdict::Confirm);
        assert_eq!(policy.navigation("GET", &post), Verdict::Allow);
        assert_eq!(policy.upload(), Verdict::Confirm);
        assert_eq!(policy.evaluate(), Verdict::Allow);
        assert_eq!(
            policy.request("POST", &post, Some("https://other.example")),
            Verdict::Allow
        );
        let open = Policy {
            preset: Preset::Open,
            ..Policy::default()
        };
        assert_eq!(open.navigation("POST", &post), Verdict::Allow);
        assert_eq!(open.upload(), Verdict::Allow);
    }

    #[test]
    fn strict_asks_before_script_sends_elsewhere() {
        let policy = Policy {
            preset: Preset::Strict,
            ..Policy::default()
        };
        let api = url("https://api.shop.example/cart");
        assert_eq!(
            policy.request("POST", &api, Some("https://www.shop.example")),
            Verdict::Allow
        );
        assert_eq!(
            policy.request("POST", &api, Some("https://tracker.example")),
            Verdict::Confirm
        );
        assert_eq!(policy.request("POST", &api, None), Verdict::Confirm);
        let socket = url("wss://live.tracker.example/socket");
        assert_eq!(
            policy.request("GET", &socket, Some("https://www.shop.example")),
            Verdict::Confirm
        );
        assert_eq!(
            policy.request(
                "GET",
                &url("wss://live.shop.example/"),
                Some("https://www.shop.example")
            ),
            Verdict::Allow
        );
        assert_eq!(policy.request("GET", &api, None), Verdict::Allow);
        assert_eq!(policy.evaluate(), Verdict::Confirm);
    }

    #[test]
    fn trusted_and_allowed_domains_cover_subdomains() {
        let policy = Policy {
            trusted: vec!["httpbin.org".into()],
            allowed_domains: vec!["example.com".into(), "httpbin.org".into()],
            ..Policy::default()
        };
        assert_eq!(
            policy.navigation("POST", &url("https://www.httpbin.org/post")),
            Verdict::Allow
        );
        assert_eq!(
            policy.navigation("GET", &url("https://docs.example.com/")),
            Verdict::Allow
        );
        assert_eq!(
            policy.navigation("GET", &url("https://notexample.com/")),
            Verdict::Block("notexample.com is not an allowed domain".into())
        );
        // A frame is a document the tab shows too.
        assert_eq!(
            policy.navigation("GET", &url("https://ads.example.net/")),
            Verdict::Block("ads.example.net is not an allowed domain".into())
        );
    }
}
