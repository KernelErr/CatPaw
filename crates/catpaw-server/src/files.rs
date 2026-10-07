//! Local files an agent uploads: where relative paths start, what they
//! are called and typed as, and how big they may be.

use std::path::{Path, PathBuf};

use crate::output::Failure;

/// The most one upload may take, all files together.
const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// A file to choose in a file input: its name, MIME type and bytes.
pub(crate) type Chosen = (String, String, Vec<u8>);

fn resolve(root: Option<&Path>, path: &str) -> PathBuf {
    let path = Path::new(path);
    match root {
        Some(root) if path.is_relative() => root.join(path),
        _ => path.to_path_buf(),
    }
}

fn mime_of(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

/// `12 B`, `3.4 KB`, `1.2 MB`.
pub(crate) fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// The names and sizes of the files at `paths` (`notes.txt (12 B)`),
/// checking that each is a file and all of them fit.
pub(crate) fn describe(root: Option<&Path>, paths: &[String]) -> Result<String, Failure> {
    let mut total = 0;
    let mut out = Vec::new();
    for path in paths {
        let full = resolve(root, path);
        let meta = std::fs::metadata(&full)
            .map_err(|e| Failure::bad_argument(format!("cannot read {path}: {e}")))?;
        if !meta.is_file() {
            return Err(Failure::bad_argument(format!("{path} is not a file")));
        }
        total += meta.len();
        let name = full
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        out.push(format!("{name} ({})", size(meta.len())));
    }
    if total > MAX_BYTES {
        return Err(Failure::bad_argument(format!(
            "the files take {}; an upload may take {}",
            size(total),
            size(MAX_BYTES)
        )));
    }
    Ok(out.join(", "))
}

/// Reads the files at `paths` for a file input.
pub(crate) fn read(root: Option<&Path>, paths: &[String]) -> Result<Vec<Chosen>, Failure> {
    describe(root, paths)?;
    paths
        .iter()
        .map(|path| {
            let full = resolve(root, path);
            let bytes = std::fs::read(&full)
                .map_err(|e| Failure::bad_argument(format!("cannot read {path}: {e}")))?;
            let name = full
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            let mime = mime_of(&name).to_string();
            Ok((name, mime, bytes))
        })
        .collect()
}
