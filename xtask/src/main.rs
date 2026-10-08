//! `cargo xtask`: developer tasks that need more than a shell one-liner.

mod bindgen;
#[cfg(feature = "wpt")]
mod harness;
mod protocol;
#[cfg(feature = "engine")]
mod snapshot_bench;
#[cfg(feature = "engine")]
mod tasks;
mod tree_construction;
mod wpt;
#[cfg(feature = "wpt")]
mod wpt_handlers;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "xtask", about = "CatPaw developer tasks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the html5lib tree-construction suite (WPT html/syntax/parsing/resources)
    /// through the CatPaw HTML parser and compare against expectations.
    TreeConstruction(tree_construction::Args),
    /// Regenerate the Web IDL bindings (or verify them with --check).
    Bindgen(bindgen::Args),
    /// Write the agent protocol to crates/catpaw-protocol/protocol.json
    /// (or verify it with --check).
    Protocol(protocol::Args),
    /// Measure snapshot and read sizes on live pages (needs `--features engine`).
    #[cfg(feature = "engine")]
    SnapshotBench(snapshot_bench::Args),
    /// The agent task set: record, replay, lint, report (needs `--features engine`).
    #[cfg(feature = "engine")]
    Tasks(tasks::Args),
    /// Run testharness.js tests from web-platform-tests in CatPaw pages and
    /// compare the results with tests/wpt-expectations (needs `--features wpt`).
    #[cfg(feature = "wpt")]
    Wpt(harness::Args),
    /// One web-platform-test in this process (what `wpt` runs per test).
    #[cfg(feature = "wpt")]
    #[command(hide = true)]
    WptOne(harness::OneArgs),
    /// Fetch the pinned web-platform-tests commit into tests/wpt-src as a sparse,
    /// blobless checkout containing the given directories.
    WptFetch {
        /// Directories (relative to the WPT root) to include in the sparse checkout.
        #[arg(required = true)]
        dirs: Vec<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::TreeConstruction(args) => tree_construction::run(args),
        Cmd::Bindgen(args) => bindgen::run(args),
        Cmd::Protocol(args) => protocol::run(args),
        #[cfg(feature = "engine")]
        Cmd::SnapshotBench(args) => snapshot_bench::run(args),
        #[cfg(feature = "engine")]
        Cmd::Tasks(args) => tasks::run_cmd(args),
        #[cfg(feature = "wpt")]
        Cmd::Wpt(args) => harness::run(args),
        #[cfg(feature = "wpt")]
        Cmd::WptOne(args) => harness::run_one(args),
        Cmd::WptFetch { dirs } => {
            let root = wpt::workspace_root();
            let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
            let path = wpt::ensure_checkout(&root, &dirs)?;
            println!("web-platform-tests checkout ready at {}", path.display());
            Ok(())
        }
    }
}
