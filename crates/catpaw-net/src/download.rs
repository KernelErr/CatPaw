//! Downloads: telling a file from a page, naming it, and saving it to
//! disk as it arrives. A navigation answered with a file is saved in the
//! client's download directory: the body goes on arriving in the
//! background, into `<name>.part`, renamed to `<name>` once complete, so
//! that a large file needs neither memory nor the page's thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http::{HeaderMap, StatusCode};
use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;
use url::Url;

/// Types a browser shows as a page; others are saved.
pub fn shows_as_page(mime: &str) -> bool {
    matches!(
        mime,
        "" | "text/html"
            | "application/xhtml+xml"
            | "text/plain"
            | "text/xml"
            | "application/xml"
            | "application/json"
            | "image/svg+xml"
    ) || mime.ends_with("+xml")
        || mime.ends_with("+json")
}

/// The MIME essence of a `Content-Type`.
fn mime(headers: &HeaderMap) -> String {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .map(|m| m.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Whether a navigation's response is a file rather than a page: an
/// attachment, a type pages are not made of, or what a `download` link
/// asked for (`asked`, its name).
pub fn is_download(status: StatusCode, headers: &HeaderMap, asked: Option<&str>) -> bool {
    if !status.is_success() {
        return false;
    }
    let attachment = headers
        .get(http::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("attachment"));
    asked.is_some() || attachment || !shows_as_page(&mime(headers))
}

/// The file name a response offers: what a `download` link asked for,
/// else `Content-Disposition`'s, else the last part of its URL.
pub fn download_name(url: &Url, headers: &HeaderMap, asked: &str) -> String {
    if !asked.trim().is_empty() {
        return asked.trim().to_string();
    }
    let header = headers
        .get(http::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for part in header.split(';').map(str::trim) {
        if let Some(name) = part.strip_prefix("filename*=") {
            let name = name.rsplit("''").next().unwrap_or(name);
            return percent_decode(name.trim_matches('"'));
        }
    }
    for part in header.split(';').map(str::trim) {
        if let Some(name) = part.strip_prefix("filename=") {
            return name.trim_matches('"').to_string();
        }
    }
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .unwrap_or_else(|| "download".to_string())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A name that is a file name on every system: no directories, no
/// characters Windows refuses, no names it reserves, not too long.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    let mut cleaned: String = cleaned.chars().take(180).collect();
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if cleaned.is_empty() || reserved {
        cleaned = format!("download{}", if cleaned.is_empty() { "" } else { "-" }) + &cleaned;
    }
    cleaned
}

/// A path in `dir` for `name` that no file has yet, nor one being saved:
/// `name`, else `name (1)`, `name (2)`… before the extension.
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, extension) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 0.. {
        let candidate = if n == 0 {
            name.to_string()
        } else {
            format!("{stem} ({n}){extension}")
        };
        let path = dir.join(&candidate);
        if !path.exists() && !part_path(&path).exists() {
            return path;
        }
    }
    unreachable!("some name is free")
}

fn part_path(path: &Path) -> PathBuf {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// What a navigation says about a download it may bring: a request
/// carrying one saves a file it is answered with in the client's download
/// directory (see [`crate::NetClient::set_download_dir`]).
#[derive(Clone, Debug, Default)]
pub struct DownloadRequest {
    /// The name a `download` link asked for, when the navigation comes
    /// from one (empty when it gives none).
    pub asked: Option<String>,
}

/// How a download stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Saving,
    Done,
    Failed(String),
}

/// A download being saved, or saved: shared between the task writing it
/// and whoever asks how it goes.
#[derive(Debug)]
pub struct DownloadProgress {
    path: PathBuf,
    total: Option<u64>,
    written: AtomicU64,
    state: std::sync::Mutex<DownloadState>,
}

impl DownloadProgress {
    /// Where the file is (complete) or will be (while saving, the bytes go
    /// to the same path with `.part` added).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The size the response declared, if it did.
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// The bytes written so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> DownloadState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, state: DownloadState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }
}

/// Starts saving `body` in `dir` under `name` (made safe and free), and
/// returns at once; the bytes go on arriving in a task of their own. A
/// body silent for `idle`, or past `limit` bytes, fails the download and
/// its partial file goes.
pub(crate) async fn save(
    dir: &Path,
    name: &str,
    total: Option<u64>,
    mut body: hyper::body::Incoming,
    limit: u64,
    idle: Duration,
) -> std::io::Result<Arc<DownloadProgress>> {
    tokio::fs::create_dir_all(dir).await?;
    let path = free_path(dir, &safe_file_name(name));
    let part = part_path(&path);
    let mut file = tokio::fs::File::create(&part).await?;
    let progress = Arc::new(DownloadProgress {
        path: path.clone(),
        total,
        written: AtomicU64::new(0),
        state: std::sync::Mutex::new(DownloadState::Saving),
    });
    let shared = progress.clone();
    tokio::spawn(async move {
        let outcome: Result<(), String> = async {
            loop {
                let frame = match tokio::time::timeout(idle, body.frame()).await {
                    Err(_) => return Err(format!("no data for {}s", idle.as_secs())),
                    Ok(None) => break,
                    Ok(Some(frame)) => frame.map_err(|e| e.to_string())?,
                };
                if let Ok(data) = frame.into_data() {
                    let written = shared.written() + data.len() as u64;
                    if written > limit {
                        return Err(format!("larger than the limit of {limit} bytes"));
                    }
                    file.write_all(&data).await.map_err(|e| e.to_string())?;
                    shared.written.store(written, Ordering::Relaxed);
                }
            }
            file.flush().await.map_err(|e| e.to_string())?;
            drop(file);
            tokio::fs::rename(&part, &path)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        match outcome {
            Ok(()) => shared.set(DownloadState::Done),
            Err(reason) => {
                let _ = tokio::fs::remove_file(&part).await;
                shared.set(DownloadState::Failed(reason));
            }
        }
    });
    Ok(progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_everywhere() {
        assert_eq!(safe_file_name("report.pdf"), "report.pdf");
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(safe_file_name("CON.txt"), "download-CON.txt");
        assert_eq!(safe_file_name("..."), "download");
        assert_eq!(safe_file_name("视频 1.mp4"), "视频 1.mp4");
    }

    #[test]
    fn a_taken_name_gets_a_number() {
        let dir = std::env::temp_dir().join(format!("catpaw-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.zip"), b"x").unwrap();
        std::fs::write(dir.join("a (1).zip.part"), b"x").unwrap();
        assert_eq!(free_path(&dir, "a.zip"), dir.join("a (2).zip"));
        assert_eq!(free_path(&dir, "b"), dir.join("b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_are_told_from_pages() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "video/mp4".parse().unwrap());
        assert!(is_download(StatusCode::OK, &headers, None));
        assert!(!is_download(StatusCode::NOT_FOUND, &headers, None));
        headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
        assert!(!is_download(StatusCode::OK, &headers, None));
        assert!(is_download(StatusCode::OK, &headers, Some("page.html")));
        headers.insert(
            "content-disposition",
            "attachment; filename*=UTF-8''%E6%96%87.txt"
                .parse()
                .unwrap(),
        );
        assert!(is_download(StatusCode::OK, &headers, None));
        let url = Url::parse("https://example.com/files/x.bin").unwrap();
        assert_eq!(download_name(&url, &headers, ""), "文.txt");
        assert_eq!(download_name(&url, &HeaderMap::new(), ""), "x.bin");
    }
}
