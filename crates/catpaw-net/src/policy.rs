//! Where requests may go: private and local addresses are refused unless
//! allowed, by checking literal hosts before a request and the addresses
//! names resolve to as the connector looks them up.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{self, Poll};

use hyper_util::client::legacy::connect::dns::Name;
use tower_service::Service;
use url::{Host, Url};

/// Whether an address belongs to the machine, its networks or a reserved
/// range rather than the public Internet.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_private_v4(v4),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
                    // Deprecated site-local range.
                    || (v6.segments()[0] & 0xffc0) == 0xfec0
                    // Documentation and discard ranges.
                    || v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8
                    || v6.segments()[0] == 0x0100 && v6.segments()[1] == 0
            }
        },
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // Carrier-grade NAT.
        || (a == 100 && (64..=127).contains(&b))
        // Reserved. The benchmarking range 198.18.0.0/15 is left alone:
        // VPN clients in "fake IP" mode hand it out for every name, and it
        // is nobody's local network.
        || a >= 240
        || a == 0
}

/// Checks a URL's host before any connection is made: literal addresses
/// and `localhost` are decided here; names are checked when resolved.
pub fn check_host(url: &Url, allow_private: bool) -> Result<(), String> {
    if allow_private {
        return Ok(());
    }
    match url.host() {
        Some(Host::Ipv4(ip)) if is_private_v4(ip) => Err(format!("{ip} is a private address")),
        Some(Host::Ipv6(ip)) if is_private_ip(IpAddr::V6(ip)) => {
            Err(format!("{ip} is a private address"))
        }
        Some(Host::Domain(name)) => {
            let name = name.trim_end_matches('.');
            if name.eq_ignore_ascii_case("localhost")
                || name.to_ascii_lowercase().ends_with(".localhost")
                || name.to_ascii_lowercase().ends_with(".local")
            {
                Err(format!("{name} is a local name"))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// A resolver for the HTTP connector that drops private addresses from
/// what a name resolves to, and fails when nothing public is left.
#[derive(Clone, Debug, Default)]
pub struct FilteringResolver {
    pub allow_private: bool,
}

impl Service<Name> for FilteringResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_string();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Self::Error)?
                .collect();
            let total = addrs.len();
            let public: Vec<SocketAddr> = addrs
                .into_iter()
                .filter(|a| allow_private || !is_private_ip(a.ip()))
                .collect();
            if public.is_empty() && total > 0 {
                return Err(format!("{host} resolves only to private addresses").into());
            }
            Ok(public.into_iter())
        })
    }
}

/// Reads a proxy URL's parts for the tunnel: its authority as a URI and
/// the `Proxy-Authorization` value its userinfo asks for.
pub fn proxy_parts(proxy: &Url) -> Result<(http::Uri, Option<http::HeaderValue>), String> {
    let host = proxy
        .host_str()
        .ok_or_else(|| "the proxy URL has no host".to_string())?;
    let port = proxy
        .port_or_known_default()
        .ok_or_else(|| "the proxy URL has no port".to_string())?;
    let scheme = match proxy.scheme() {
        "http" | "socks5" | "socks5h" => proxy.scheme(),
        "https" => "https",
        other => return Err(format!("unsupported proxy scheme `{other}`")),
    };
    let uri: http::Uri = format!("{scheme}://{host}:{port}")
        .parse()
        .map_err(|e| format!("invalid proxy URL: {e}"))?;
    let auth = if !proxy.username().is_empty() {
        use base64::Engine as _;
        let credentials = format!(
            "{}:{}",
            percent_decode(proxy.username()),
            percent_decode(proxy.password().unwrap_or_default())
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
        Some(
            http::HeaderValue::from_str(&format!("Basic {encoded}"))
                .map_err(|e| format!("invalid proxy credentials: {e}"))?,
        )
    } else {
        None
    };
    Ok((uri, auth))
}

fn percent_decode(input: &str) -> String {
    url::form_urlencoded::parse(input.as_bytes())
        .map(|(k, v)| format!("{k}{v}"))
        .collect::<Vec<_>>()
        .join("")
        .replace('+', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_are_recognised() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "2606:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn literal_hosts_are_checked_up_front() {
        let bad = Url::parse("http://127.0.0.1:8080/").unwrap();
        assert!(check_host(&bad, false).is_err());
        assert!(check_host(&bad, true).is_ok());
        let local = Url::parse("http://localhost/").unwrap();
        assert!(check_host(&local, false).is_err());
        let six = Url::parse("http://[::1]/").unwrap();
        assert!(check_host(&six, false).is_err());
        let fine = Url::parse("https://example.com/").unwrap();
        assert!(check_host(&fine, false).is_ok());
    }

    #[test]
    fn proxy_urls_give_a_uri_and_credentials() {
        let (uri, auth) =
            proxy_parts(&Url::parse("http://user:p%40ss@proxy.example:3128").unwrap()).unwrap();
        assert_eq!(uri.to_string(), "http://proxy.example:3128/");
        assert_eq!(auth.unwrap().to_str().unwrap(), "Basic dXNlcjpwQHNz");
        let (uri, auth) = proxy_parts(&Url::parse("http://proxy.example").unwrap()).unwrap();
        assert_eq!(uri.to_string(), "http://proxy.example:80/");
        assert!(auth.is_none());
        assert!(proxy_parts(&Url::parse("ftp://proxy.example").unwrap()).is_err());
    }
}
