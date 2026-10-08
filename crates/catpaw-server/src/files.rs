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

/// The files at `paths`, links followed, with their sizes; checks that
/// each is a file and that all of them fit.
fn check(root: Option<&Path>, paths: &[String]) -> Result<Vec<(PathBuf, u64)>, Failure> {
    let mut total = 0;
    let mut out = Vec::new();
    for path in paths {
        let unreadable =
            |e: std::io::Error| Failure::bad_argument(format!("cannot read {path}: {e}"));
        let real = std::fs::canonicalize(resolve(root, path)).map_err(unreadable)?;
        let meta = std::fs::metadata(&real).map_err(unreadable)?;
        if !meta.is_file() {
            return Err(Failure::bad_argument(format!("{path} is not a file")));
        }
        total += meta.len();
        out.push((real, meta.len()));
    }
    if total > MAX_BYTES {
        return Err(Failure::bad_argument(format!(
            "the files take {}; an upload may take {}",
            size(total),
            size(MAX_BYTES)
        )));
    }
    Ok(out)
}

/// Whether any of `paths` is the file at `secret`: the same file under
/// whatever name or link, or a copy of it (a small file with the same
/// bytes).
pub(crate) fn names_file(root: Option<&Path>, paths: &[String], secret: &Path) -> bool {
    let Ok(secret_meta) = std::fs::metadata(secret) else {
        return false;
    };
    let real_secret = std::fs::canonicalize(secret).ok();
    let secret_bytes = (secret_meta.len() <= 4096)
        .then(|| std::fs::read(secret).ok())
        .flatten();
    paths.iter().any(|path| {
        let full = resolve(root, path);
        if real_secret.is_some() && std::fs::canonicalize(&full).ok() == real_secret {
            return true;
        }
        let Ok(meta) = std::fs::metadata(&full) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.dev() == secret_meta.dev() && meta.ino() == secret_meta.ino() {
                return true;
            }
        }
        meta.len() == secret_meta.len()
            && secret_bytes
                .as_ref()
                .is_some_and(|bytes| std::fs::read(&full).ok().as_ref() == Some(bytes))
    })
}

/// The name a file goes by in the page: the last part of the path given.
fn name_of(root: Option<&Path>, path: &str) -> String {
    resolve(root, path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// The names and sizes of the files at `paths` (`notes.txt (12 B)`),
/// checking that each is a file and all of them fit.
pub(crate) fn describe(root: Option<&Path>, paths: &[String]) -> Result<String, Failure> {
    let files = check(root, paths)?;
    Ok(paths
        .iter()
        .zip(files)
        .map(|(path, (_, bytes))| format!("{} ({})", name_of(root, path), size(bytes)))
        .collect::<Vec<_>>()
        .join(", "))
}

/// A path as people write it: without the `\\?\` prefix Windows gives
/// canonical paths (`\\?\UNC\server\share` reads `\\server\share`).
fn plain(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    }
}

/// The files at `paths` as the user approves them: where each really is
/// (links followed; relative to the files root when under it, else with
/// the home directory as `~`) and its size.
pub(crate) fn describe_paths(root: Option<&Path>, paths: &[String]) -> Result<String, Failure> {
    let files = check(root, paths)?;
    let real_root = root.and_then(|r| std::fs::canonicalize(r).ok());
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(|h| std::fs::canonicalize(h).ok());
    Ok(files
        .iter()
        .map(|(real, bytes)| {
            let shown = match (&real_root, &home) {
                (Some(root), _) if real.starts_with(root) => real
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                (_, Some(home)) if real.starts_with(home) => match real.strip_prefix(home) {
                    Ok(rest) => format!("~/{}", rest.display()),
                    Err(_) => plain(real),
                },
                _ => plain(real),
            };
            format!("{shown} ({})", size(*bytes))
        })
        .collect::<Vec<_>>()
        .join(", "))
}

/// Reads the files at `paths` for a file input.
pub(crate) fn read(root: Option<&Path>, paths: &[String]) -> Result<Vec<Chosen>, Failure> {
    let files = check(root, paths)?;
    paths
        .iter()
        .zip(files)
        .map(|(path, (real, _))| {
            let bytes = std::fs::read(&real)
                .map_err(|e| Failure::bad_argument(format!("cannot read {path}: {e}")))?;
            let name = name_of(root, path);
            let mime = mime_of(&name).to_string();
            Ok((name, mime, bytes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_show_without_the_verbatim_prefix() {
        assert_eq!(
            plain(Path::new(r"\\?\D:\trial\hello.txt")),
            r"D:\trial\hello.txt"
        );
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share\a.txt")),
            r"\\server\share\a.txt"
        );
        assert_eq!(plain(Path::new("/srv/a.txt")), "/srv/a.txt");
    }

    #[test]
    fn an_upload_shows_where_each_file_really_is() {
        let base = std::env::temp_dir().join(format!("catpaw-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, elsewhere) = (base.join("root"), base.join("elsewhere"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(root.join("notes.txt"), "twelve bytes").unwrap();
        std::fs::write(elsewhere.join("id.txt"), "ssh").unwrap();
        let outside = elsewhere.join("id.txt").display().to_string();
        let shown = describe_paths(Some(&root), &["notes.txt".to_string(), outside]).unwrap();
        // Under the root, relative to it; elsewhere, all of the path (from
        // the home directory, when it is under it), so that the user sees
        // which file leaves the machine.
        let (inside, outside) = shown.split_once(", ").unwrap();
        assert_eq!(inside, "notes.txt (12 B)");
        let outside = outside.strip_suffix(" (3 B)").unwrap();
        assert!(outside.ends_with("id.txt"), "{outside}");
        assert!(
            outside.starts_with('~') || Path::new(outside).is_absolute(),
            "{outside}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
