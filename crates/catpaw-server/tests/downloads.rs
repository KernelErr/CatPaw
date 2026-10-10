//! Downloads are saved to the download directory as they arrive: past the
//! limit responses have in memory, the agent waiting for them and reading
//! where they went.

use std::io::{Read, Write};
use std::net::TcpListener;

use catpaw_server::{McpServer, Session, SessionConfig, Settings};
use serde_json::{Value, json};

const FILE_SIZE: usize = 3 * 1024 * 1024;

/// Serves a page linking to a file, and the file (an attachment).
fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                if head.starts_with("GET /file.bin") {
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"report.bin\"\r\nContent-Length: {FILE_SIZE}\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                    let chunk = vec![7u8; 64 * 1024];
                    for _ in 0..FILE_SIZE / chunk.len() {
                        if stream.write_all(&chunk).is_err() {
                            return;
                        }
                    }
                } else {
                    let body = "<!doctype html><title>Files</title><a href=\"/file.bin\">Get the report</a>";
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                }
            });
        }
    });
    port
}

fn call(server: &mut McpServer, id: u64, name: &str, args: Value) -> String {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let reply: Value =
        serde_json::from_str(&server.handle_line(&line.to_string()).unwrap()).unwrap();
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_file_past_the_memory_limit_is_saved_to_the_download_directory() {
    let port = serve();
    let dir = std::env::temp_dir().join(format!("catpaw-downloads-{}", std::process::id()));
    let mut config = SessionConfig::default();
    config.options.net.allow_private_network = true;
    // Responses in memory stop at 1 MB; the file is three.
    config.options.net.max_response_bytes = 1024 * 1024;
    config.settings = Settings {
        download_dir: Some(dir.clone()),
        ..Settings::default()
    };
    let mut server = McpServer::new(Session::new(config).unwrap());

    let page = call(
        &mut server,
        1,
        "navigate",
        json!({"url": format!("http://127.0.0.1:{port}/")}),
    );
    let link = page
        .lines()
        .find(|l| l.contains("Get the report"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();
    let clicked = call(&mut server, 2, "click", json!({"target": link}));
    assert!(clicked.contains("\"report.bin\""), "{clicked}");
    assert!(clicked.contains("saving to"), "{clicked}");

    let waited = call(
        &mut server,
        3,
        "wait",
        json!({"for": "download", "timeoutMs": 30000}),
    );
    assert!(
        waited.starts_with("ok wait download: \"report.bin\" saved to"),
        "{waited}"
    );
    let saved = dir.join("report.bin");
    assert_eq!(std::fs::metadata(&saved).unwrap().len() as usize, FILE_SIZE);
    assert!(!dir.join("report.bin.part").exists());

    let read = call(&mut server, 4, "read", json!({"view": "download"}));
    assert!(
        read.contains("saved to") && read.contains("(not text)"),
        "{read}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
