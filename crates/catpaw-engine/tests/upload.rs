//! File inputs: choosing files, what script sees of them, and the
//! multipart submission that carries them, against a local server that
//! answers with the body it received.

use std::io::{Read, Write};
use std::net::TcpListener;

use catpaw_engine::{FrameId, LoopLimits, PageOptions, with_page};
use catpaw_net::NetConfig;
use url::Url;

const FORM: &str = "<!doctype html><form method=post enctype=multipart/form-data action=/up><input type=file name=doc id=f multiple><input name=note value=hi><button id=go>Go</button></form><p id=out></p><script>document.getElementById('f').addEventListener('change', e => { const f = e.target.files; document.getElementById('out').textContent = f.length + ' ' + f[0].name + ' ' + f.item(1).size + ' ' + e.target.value; });</script>";

/// Serves the form at `/form`; a POST is answered with its own body.
fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut data = Vec::new();
            let mut buf = vec![0u8; 65536];
            // Read the head, then as much body as it announces.
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&data);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&data).into_owned();
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
            let reply = if head.starts_with("POST") {
                let kind = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
                    .unwrap_or("")
                    .to_string();
                format!(
                    "<!doctype html><pre id=got>{}\n{}</pre>",
                    kind.replace('<', "&lt;"),
                    body.replace('<', "&lt;")
                )
            } else {
                FORM.to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

#[test]
fn chosen_files_reach_script_and_the_server() {
    let port = serve();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/form")).unwrap();
    let options = PageOptions {
        net: NetConfig {
            allow_private_network: true,
            ..NetConfig::default()
        },
        limits: LoopLimits {
            wall: std::time::Duration::from_secs(10),
            virtual_ms: 5_000.0,
            max_steps: 100_000,
            settle: None,
        },
        ..PageOptions::default()
    };
    with_page(url, options, |page| {
        assert_eq!(
            page.eval("[document.getElementById('f').files.length, document.getElementById('f').value, document.querySelector('[name=note]').files]")
                .unwrap(),
            "[ 0, '', null ]"
        );
        let input = page.find("#f").unwrap();
        page.input_in(FrameId(0), |cx| {
            catpaw_web::input::choose_files(
                cx,
                input,
                vec![
                    ("notes.txt".to_string(), "text/plain".to_string(), b"hello upload".to_vec()),
                    ("b.bin".to_string(), String::new(), vec![0, 1, 2]),
                ],
            )
        })
        .unwrap();
        assert_eq!(
            page.eval("document.getElementById('out').textContent").unwrap(),
            "2 notes.txt 3 C:\\fakepath\\notes.txt"
        );
        assert_eq!(
            page.eval("document.getElementById('f').files === document.getElementById('f').files")
                .unwrap(),
            "true"
        );
        page.click("#go").unwrap();
        let got = page.eval("document.getElementById('got').textContent").unwrap();
        assert!(got.contains("multipart/form-data; boundary="), "{got}");
        // The parser turned the body's CRLFs into LFs.
        assert!(
            got.contains("Content-Disposition: form-data; name=\"doc\"; filename=\"notes.txt\"\nContent-Type: text/plain\n\nhello upload"),
            "{got}"
        );
        assert!(got.contains("filename=\"b.bin\""), "{got}");
        assert!(got.contains("name=\"note\"\n\nhi"), "{got}");
    })
    .unwrap();
}

#[test]
fn a_required_file_is_needed_and_reset_clears_the_choice() {
    let port = serve();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/required")).unwrap();
    let html = "<!doctype html><form id=form method=post enctype=multipart/form-data action=/up><input type=file name=doc id=f required><button id=go>Go</button></form>".to_string();
    let options = PageOptions {
        net: NetConfig {
            allow_private_network: true,
            ..NetConfig::default()
        },
        ..PageOptions::default()
    };
    catpaw_engine::with_html(url, html, options, |page| {
        let valid = "document.getElementById('form').checkValidity()";
        assert_eq!(page.eval(valid).unwrap(), "false");
        page.click("#go").unwrap();
        assert!(
            page.url().path().ends_with("/required"),
            "a required file input with no file stops the submission"
        );
        let input = page.find("#f").unwrap();
        page.input_in(FrameId(0), |cx| {
            catpaw_web::input::choose_files(
                cx,
                input,
                vec![("a.txt".to_string(), "text/plain".to_string(), b"a".to_vec())],
            )
        })
        .unwrap();
        assert_eq!(page.eval(valid).unwrap(), "true");
        page.eval("document.getElementById('form').reset()")
            .unwrap();
        assert_eq!(
            page.eval("document.getElementById('f').files.length")
                .unwrap(),
            "0"
        );
        assert_eq!(page.eval(valid).unwrap(), "false");
    })
    .unwrap();
}
