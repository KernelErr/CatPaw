//! Connections go through the proxy for their target's scheme, or
//! directly for the hosts `no_proxy` names.

use std::sync::{Arc, Mutex};

use catpaw_net::{NetClient, NetConfig, NoProxy, Proxies, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A name only the proxy below can reach (`.invalid` never resolves), so
/// that a request reaches the server through the proxy or not at all.
/// (Loopback targets never take a proxy.)
const HOST: &str = "upstream.invalid";

/// Reads up to the end of a request head.
async fn read_head(socket: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await.unwrap_or(0) == 0 {
            break;
        }
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head).to_string()
}

/// A server that answers every request with `hello`.
async fn server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                read_head(&mut socket).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                    )
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    port
}

/// A `CONNECT` proxy that notes each target and tunnels it to this
/// machine, on the target's port.
async fn proxy(seen: Arc<Mutex<Vec<String>>>) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let head = read_head(&mut socket).await;
                let target = head
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("CONNECT "))
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or_default()
                    .to_string();
                seen.lock().unwrap().push(target.clone());
                let port = target.rsplit(':').next().unwrap_or_default();
                let Ok(mut upstream) = TcpStream::connect(format!("127.0.0.1:{port}")).await else {
                    return;
                };
                let _ = socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
            });
        }
    });
    Url::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

async fn get(proxies: Proxies, port: u16) -> Result<String, String> {
    let client = NetClient::new(NetConfig {
        proxy: proxies,
        ..NetConfig::default()
    })
    .unwrap();
    let url = Url::parse(&format!("http://{HOST}:{port}/")).unwrap();
    let response = client.get(&url).await.map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&response.body).to_string())
}

#[tokio::test]
async fn targets_take_their_scheme_s_proxy_unless_no_proxy_names_them() {
    let port = server().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let via = proxy(seen.clone()).await;
    let target = format!("{HOST}:{port}");

    // An HTTP target, through the HTTP proxy, which resolves the name.
    let http_proxy = Proxies {
        http: Some(via.clone()),
        ..Proxies::default()
    };
    assert_eq!(get(http_proxy.clone(), port).await.unwrap(), "hello");
    assert_eq!(*seen.lock().unwrap(), std::slice::from_ref(&target));

    // Its host in `no_proxy`: directly, where the name does not resolve.
    let bypassed = Proxies {
        bypass: NoProxy::parse(".invalid"),
        ..http_proxy
    };
    assert!(get(bypassed, port).await.is_err());
    // An HTTPS proxy is not for HTTP targets.
    let https_only = Proxies {
        https: Some(via),
        ..Proxies::default()
    };
    assert!(get(https_only, port).await.is_err());
    assert_eq!(
        *seen.lock().unwrap(),
        [target],
        "only the first went through"
    );
}
