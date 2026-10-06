//! Web IDL tooling for CatPaw: an owned IDL model (parsed with `weedle`),
//! the binding manifest, and the emitters that turn both into the
//! engine-neutral interface traits and the per-engine glue. Driven by
//! `cargo xtask bindgen`; see ADR 0004.

pub mod emit;
pub mod model;
pub mod names;
pub mod plan;

pub use model::{Idl, Interface, InterfaceKind, Member, Type};

/// Loads every `.idl` file in a directory (sorted by name) into one corpus.
pub fn load_dir(idl: &mut Idl, dir: &std::path::Path) -> anyhow::Result<()> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "idl"))
        .collect();
    files.sort();
    for file in files {
        let text = std::fs::read_to_string(&file)?;
        idl.load(&file.file_name().unwrap().to_string_lossy(), &text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_vendored_corpus() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("idl/webref");
        let mut idl = Idl::new();
        load_dir(&mut idl, &dir).unwrap();
        load_dir(&mut idl, &dir.join("../overlay")).unwrap();
        assert!(idl.interfaces.contains_key("Node"));
        assert!(idl.interfaces.contains_key("HTMLInputElement"));
        assert!(idl.interfaces["Window"].members.len() > 50);
    }
}
