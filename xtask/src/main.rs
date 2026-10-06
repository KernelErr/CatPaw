//! `cargo xtask`: developer tasks that need more than a shell one-liner.

mod tree_construction;
mod wpt;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
        Cmd::WptFetch { dirs } => {
            let root = wpt::workspace_root();
            let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
            let path = wpt::ensure_checkout(&root, &dirs)?;
            println!("web-platform-tests checkout ready at {}", path.display());
            Ok(())
        }
    }
}
