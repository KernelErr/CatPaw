//! Limits and policy: oversized bodies are refused, private addresses are
//! off unless allowed.

use std::io::Write;

use catpaw_net::{NetClient, NetConfig, NetError, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves one HTTP/1.1 response and closes.
async fn serve_once(status: &'static str, headers: &'static str, body: Vec<u8>) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0u8; 4096];
        let _ = socket.read(&mut request).await;
        let head = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(&body).await.unwrap();
        socket.shutdown().await.ok();
    });
    Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap()
}

fn local_client(config: NetConfig) -> NetClient {
    NetClient::new(NetConfig {
        allow_private_network: true,
        ..config
    })
    .unwrap()
}

#[tokio::test]
async fn private_addresses_are_refused_unless_allowed() {
    let client = NetClient::new(NetConfig::default()).unwrap();
    for url in [
        "http://127.0.0.1:1/",
        "http://localhost:1/",
        "http://[::1]:1/",
        "http://10.0.0.1/",
    ] {
        let err = client.get(&Url::parse(url).unwrap()).await.unwrap_err();
        assert!(matches!(err, NetError::PrivateAddress(_)), "{url}: {err}");
    }
    // Allowed, the request goes out (and fails to connect to port 1).
    let client = local_client(NetConfig::default());
    let err = client
        .get(&Url::parse("http://127.0.0.1:1/").unwrap())
        .await
        .unwrap_err();
    assert!(!matches!(err, NetError::PrivateAddress(_)), "{err}");
}

#[tokio::test]
async fn local_names_are_refused_through_a_proxy_too() {
    // A proxy on this machine would otherwise reach its services.
    let client = NetClient::new(NetConfig {
        proxy: Some(Url::parse("http://127.0.0.1:9").unwrap()),
        ..NetConfig::default()
    })
    .unwrap();
    for url in [
        "http://localhost:1/",
        "http://printer.local/",
        "http://127.0.0.1:1/",
    ] {
        let err = client.get(&Url::parse(url).unwrap()).await.unwrap_err();
        assert!(matches!(err, NetError::PrivateAddress(_)), "{url}: {err}");
    }
}

#[tokio::test]
async fn bodies_past_the_wire_limit_are_refused() {
    let url = serve_once(
        "200 OK",
        "Content-Type: text/plain\r\n",
        vec![b'x'; 100_000],
    )
    .await;
    let client = local_client(NetConfig {
        max_response_bytes: 50_000,
        ..NetConfig::default()
    });
    let err = client.get(&url).await.unwrap_err();
    assert!(matches!(err, NetError::TooLarge(50_000)), "{err}");

    let url = serve_once(
        "200 OK",
        "Content-Type: text/plain\r\n",
        vec![b'x'; 100_000],
    )
    .await;
    let client = local_client(NetConfig {
        max_response_bytes: 200_000,
        ..NetConfig::default()
    });
    let response = client.get(&url).await.unwrap();
    assert_eq!(response.body.len(), 100_000);
}

#[tokio::test]
async fn bodies_that_decompress_past_the_limit_are_refused() {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&vec![0u8; 4 * 1024 * 1024]).unwrap();
    let bomb = enc.finish().unwrap();
    assert!(bomb.len() < 16 * 1024, "{}", bomb.len());
    let url = serve_once("200 OK", "Content-Encoding: gzip\r\n", bomb).await;
    let client = local_client(NetConfig {
        max_decoded_bytes: 1024 * 1024,
        ..NetConfig::default()
    });
    let err = client.get(&url).await.unwrap_err();
    assert!(matches!(err, NetError::TooLarge(1_048_576)), "{err}");
}

#[test]
fn cookie_jars_round_trip_through_json() {
    let jar = catpaw_net::CookieJar::new();
    let url = Url::parse("https://example.com/path").unwrap();
    let mut headers = http::HeaderMap::new();
    headers.append(
        http::header::SET_COOKIE,
        "session=abc; Path=/; HttpOnly".parse().unwrap(),
    );
    headers.append(
        http::header::SET_COOKIE,
        "theme=dark; Path=/; Max-Age=3600".parse().unwrap(),
    );
    jar.store_response(&url, &headers);
    let json = jar.to_json();
    let copy = catpaw_net::CookieJar::new();
    assert_eq!(copy.load_json(&json).unwrap(), 2);
    let header = copy.request_header(&url).unwrap();
    assert!(
        header.contains("session=abc") && header.contains("theme=dark"),
        "{header}"
    );
}
