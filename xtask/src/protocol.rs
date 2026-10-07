//! `cargo xtask protocol`: writes the agent protocol (tools, schemas,
//! instructions) to `crates/catpaw-protocol/protocol.json`, or checks
//! that the file is current.

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

use crate::wpt::workspace_root;

#[derive(ClapArgs)]
pub struct Args {
    /// Fail if protocol.json differs from what the code says, instead of
    /// writing it.
    #[arg(long)]
    check: bool,
}

pub fn run(args: Args) -> Result<()> {
    let path = workspace_root().join("crates/catpaw-protocol/protocol.json");
    let fresh = catpaw_protocol::protocol_json();
    if args.check {
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != fresh {
            bail!(
                "crates/catpaw-protocol/protocol.json is out of date; run `cargo xtask protocol`"
            );
        }
        println!("protocol.json is up to date");
        return Ok(());
    }
    std::fs::write(&path, fresh).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote crates/catpaw-protocol/protocol.json");
    Ok(())
}
