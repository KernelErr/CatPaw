//! A pinned, sparse, blobless checkout of web-platform-tests.
//!
//! WPT is far too large to vendor or to add as a submodule, so test runners
//! ask for the directories they need and this module materialises exactly
//! those at the commit recorded in `tests/wpt.lock`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const WPT_REPO: &str = "https://github.com/web-platform-tests/wpt.git";

/// The workspace root (the parent of the `xtask` crate).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives directly under the workspace root")
        .to_path_buf()
}

pub fn checkout_dir(root: &Path) -> PathBuf {
    root.join("tests").join("wpt-src")
}

/// The commit pinned in `tests/wpt.lock` (first non-comment line).
pub fn pinned_commit(root: &Path) -> Result<String> {
    let lock = root.join("tests").join("wpt.lock");
    let text =
        std::fs::read_to_string(&lock).with_context(|| format!("reading {}", lock.display()))?;
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .context("tests/wpt.lock does not name a commit")
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`git {}` failed in {}", args.join(" "), dir.display());
    }
    Ok(())
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!("`git {}` failed in {}", args.join(" "), dir.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Ensures `tests/wpt-src` exists at the pinned commit with at least `dirs`
/// present in its sparse-checkout set. Returns the checkout path.
pub fn ensure_checkout(root: &Path, dirs: &[&str]) -> Result<PathBuf> {
    let dir = checkout_dir(root);
    let sha = pinned_commit(root)?;

    if !dir.join(".git").exists() {
        std::fs::create_dir_all(&dir)?;
        git(&dir, &["init", "-q"])?;
        // Test fixtures must keep their exact bytes: never translate line endings.
        git(&dir, &["config", "core.autocrlf", "false"])?;
        git(&dir, &["config", "core.eol", "lf"])?;
        git(&dir, &["remote", "add", "origin", WPT_REPO])?;
        git(&dir, &["sparse-checkout", "init", "--cone"])?;
    }

    let mut wanted: BTreeSet<String> = git_output(&dir, &["sparse-checkout", "list"])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    for d in dirs {
        wanted.insert((*d).to_string());
    }
    let mut args = vec!["sparse-checkout", "set"];
    args.extend(wanted.iter().map(String::as_str));
    git(&dir, &args)?;

    let head = git_output(&dir, &["rev-parse", "--verify", "-q", "HEAD"]).unwrap_or_default();
    if head.trim() != sha {
        eprintln!(
            "xtask: fetching web-platform-tests @ {sha} (sparse: {})",
            wanted.iter().cloned().collect::<Vec<_>>().join(", ")
        );
        git(
            &dir,
            &[
                "fetch",
                "-q",
                "--filter=blob:none",
                "--depth",
                "1",
                "origin",
                &sha,
            ],
        )?;
    }
    // `checkout` also materialises directories newly added to the sparse set.
    git(&dir, &["checkout", "-q", "--detach", &sha])?;
    Ok(dir)
}
