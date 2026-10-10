//! Which proxy a connection goes through: one for every connection (the
//! CLI's `--proxy`), or the ones the environment names, read as curl reads
//! them (`https_proxy`, `http_proxy`, `all_proxy`, `no_proxy`).

use std::net::IpAddr;

use url::Url;

/// The proxies connections go through. Empty, every connection is direct.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proxies {
    /// For `https:` and `wss:` targets.
    pub https: Option<Url>,
    /// For `http:` and `ws:` targets.
    pub http: Option<Url>,
    /// Hosts reached directly whatever the above.
    pub bypass: NoProxy,
}

impl Proxies {
    /// One proxy for every connection.
    pub fn all(proxy: Url) -> Self {
        Self {
            https: Some(proxy.clone()),
            http: Some(proxy),
            bypass: NoProxy::default(),
        }
    }

    /// The proxies the environment names, as curl reads them:
    /// `https_proxy` for HTTPS targets and `http_proxy` for HTTP ones,
    /// `all_proxy` for either where its own is not set, and `no_proxy` for
    /// the hosts reached directly. The lower-case name wins over the upper
    /// case one, an empty value counts as unset, and a value without a
    /// scheme is an HTTP proxy. `env` gives a variable's value.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        // The variable set (lower case first) and its value.
        let var = |name: &str| {
            [name.to_string(), name.to_ascii_uppercase()]
                .into_iter()
                .find_map(|name| {
                    let value = env(&name)?.trim().to_string();
                    (!value.is_empty()).then_some((name, value))
                })
        };
        let parse = |name: &str| -> Result<Option<Url>, String> {
            var(name)
                .map(|(name, value)| proxy_url(&name, &value))
                .transpose()
        };
        let all = parse("all_proxy")?;
        Ok(Self {
            https: parse("https_proxy")?.or_else(|| all.clone()),
            http: parse("http_proxy")?.or(all),
            bypass: var("no_proxy")
                .map(|(_, list)| NoProxy::parse(&list))
                .unwrap_or_default(),
        })
    }

    /// Whether every connection is direct.
    pub fn is_empty(&self) -> bool {
        self.https.is_none() && self.http.is_none()
    }

    /// The proxy for a connection to `host` for a target of `scheme`
    /// (`https` or `wss`, else HTTP), or `None` to connect directly. This
    /// machine (`localhost`, loopback addresses) is always reached
    /// directly, as Chrome does.
    pub fn for_target(&self, scheme: &str, host: &str) -> Option<&Url> {
        let proxy = match scheme {
            "https" | "wss" => self.https.as_ref(),
            _ => self.http.as_ref(),
        }?;
        (!is_loopback(host) && !self.bypass.matches(host)).then_some(proxy)
    }
}

/// A proxy URL from the environment variable `name`.
fn proxy_url(name: &str, value: &str) -> Result<Url, String> {
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    // The value may hold a password: errors name the variable only.
    let url = Url::parse(&value).map_err(|e| format!("{name} is not a proxy URL ({e})"))?;
    match url.scheme() {
        "http" | "socks5" | "socks5h" => Ok(url),
        other => Err(format!(
            "{name} names a proxy of scheme `{other}`; http, socks5 and socks5h are supported \
             (or pass --proxy, or --proxy direct to ignore the environment)"
        )),
    }
}

/// The hosts reached without a proxy (`no_proxy`): `*` for all of them;
/// a name for itself and its subdomains (`example.com`, `.example.com`
/// and `*.example.com` alike); an IP address; or a network of them
/// (`10.0.0.0/8`). A port after an entry is ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoProxy {
    everything: bool,
    names: Vec<String>,
    networks: Vec<(IpAddr, u8)>,
}

impl NoProxy {
    pub fn parse(list: &str) -> Self {
        let mut out = Self::default();
        for entry in list
            .split([',', ' '])
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            if entry == "*" {
                out.everything = true;
                continue;
            }
            let (address, bits) = match entry.split_once('/') {
                Some((address, bits)) => (address, bits.parse::<u8>().ok()),
                None => (entry, None),
            };
            let address = address.trim_start_matches('[').trim_end_matches(']');
            if let Ok(ip) = address.parse::<IpAddr>() {
                let max = if ip.is_ipv4() { 32 } else { 128 };
                out.networks.push((ip, bits.unwrap_or(max).min(max)));
                continue;
            }
            // `host:port` (a name has no colon of its own).
            let name = entry.split(':').next().unwrap_or(entry);
            let name = name.trim_start_matches("*.").trim_start_matches('.');
            if !name.is_empty() {
                out.names.push(name.to_ascii_lowercase());
            }
        }
        out
    }

    /// Whether `host` (a name, or an address, IPv6 in brackets or not) is
    /// reached directly.
    pub fn matches(&self, host: &str) -> bool {
        if self.everything {
            return true;
        }
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.');
        if let Ok(ip) = host.parse::<IpAddr>() {
            return self
                .networks
                .iter()
                .any(|&(network, bits)| in_network(ip, network, bits));
        }
        let host = host.to_ascii_lowercase();
        self.names.iter().any(|name| {
            host == *name
                || host
                    .strip_suffix(name.as_str())
                    .is_some_and(|rest| rest.ends_with('.'))
        })
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host == "localhost" || host.ends_with(".localhost"),
    }
}

fn in_network(ip: IpAddr, network: IpAddr, bits: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn from(vars: &[(&str, &str)]) -> Result<Proxies, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Proxies::from_env(|name| vars.get(name).cloned())
    }

    #[test]
    fn the_environment_is_read_as_curl_reads_it() {
        assert!(from(&[]).unwrap().is_empty());
        let p = from(&[("HTTPS_PROXY", "127.0.0.1:7890")]).unwrap();
        assert_eq!(p.https.unwrap().as_str(), "http://127.0.0.1:7890/");
        assert_eq!(p.http, None, "HTTPS_PROXY is for HTTPS targets only");

        let p = from(&[
            ("all_proxy", "socks5h://proxy:1080"),
            ("http_proxy", "http://web:3128"),
            ("HTTP_PROXY", "http://ignored:1"),
            ("https_proxy", ""),
        ])
        .unwrap();
        assert_eq!(p.https.unwrap().as_str(), "socks5h://proxy:1080");
        assert_eq!(p.http.unwrap().as_str(), "http://web:3128/");

        let err = from(&[("https_proxy", "socks4://user:secret@proxy:1080")]).unwrap_err();
        assert!(
            err.contains("https_proxy") && err.contains("socks4") && !err.contains("secret"),
            "{err}"
        );
        let err = from(&[("ALL_PROXY", "ftp://proxy:21")]).unwrap_err();
        assert!(err.starts_with("ALL_PROXY "), "{err}");
    }

    #[test]
    fn targets_pick_their_proxy_and_no_proxy_wins() {
        let p = from(&[
            ("https_proxy", "http://s:1"),
            ("http_proxy", "http://p:2"),
            (
                "no_proxy",
                "localhost, .internal.example,10.0.0.0/8, ::1, example.org:8080",
            ),
        ])
        .unwrap();
        let host = |scheme, host| p.for_target(scheme, host).and_then(Url::host_str);
        assert_eq!(host("https", "catpaw.sh"), Some("s"));
        assert_eq!(host("wss", "catpaw.sh"), Some("s"));
        assert_eq!(host("http", "catpaw.sh"), Some("p"));
        for direct in [
            "localhost",
            "internal.example",
            "api.internal.example",
            "10.1.2.3",
            "[::1]",
            "example.org",
            "WWW.Example.org",
        ] {
            assert_eq!(host("https", direct), None, "{direct}");
        }
        for proxied in ["notinternal.example", "11.0.0.1", "example.org.evil", "org"] {
            assert_eq!(host("https", proxied), Some("s"), "{proxied}");
        }
        assert!(NoProxy::parse("*").matches("anything.example"));
        // This machine never goes through a proxy.
        let all = Proxies::all(Url::parse("http://proxy:1").unwrap());
        for local in [
            "localhost",
            "app.localhost",
            "127.0.0.1",
            "127.8.0.1",
            "[::1]",
        ] {
            assert_eq!(all.for_target("http", local), None, "{local}");
        }
    }
}
