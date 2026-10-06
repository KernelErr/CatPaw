//! `cargo xtask bindgen`: regenerate the Web IDL bindings.
//!
//! Inputs: the IDL corpus under `crates/catpaw-webidl/idl/` and the manifest
//! `crates/catpaw-webidl/bindings.toml`. Outputs (checked in):
//! `crates/catpaw-web/src/generated.rs` (engine-neutral traits and types) and
//! `crates/catpaw-bindings-boa/src/generated.rs` (Boa glue).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use catpaw_webidl::emit::Emitter;
use catpaw_webidl::model::Member;
use catpaw_webidl::plan::{Manifest, Planner};
use catpaw_webidl::{Idl, load_dir};

use crate::wpt::workspace_root;

#[derive(clap::Args)]
pub struct Args {
    /// Verify the checked-in output is up to date instead of writing it.
    #[arg(long)]
    pub check: bool,
    /// Print the members an interface (with its mixins) offers and exit.
    #[arg(long)]
    pub list: Option<String>,
}

fn rustfmt(code: &str) -> Result<String> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running rustfmt")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = code.to_string();
    // Feed stdin from another thread so a large output cannot deadlock the pipes.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !out.status.success() {
        bail!(
            "rustfmt rejected the generated code:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn load(root: &Path) -> Result<(Idl, Manifest)> {
    let base = root.join("crates").join("catpaw-webidl");
    let mut idl = Idl::new();
    load_dir(&mut idl, &base.join("idl").join("webref"))?;
    load_dir(&mut idl, &base.join("idl").join("overlay"))?;
    let manifest_path = base.join("bindings.toml");
    let manifest = Manifest::parse(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?,
    )
    .context("parsing bindings.toml")?;
    Ok((idl, manifest))
}

fn list(idl: &Idl, manifest: &Manifest, name: &str) -> Result<()> {
    if !idl.interfaces.contains_key(name) {
        bail!("no interface, mixin or namespace named `{name}`");
    }
    let mut owners = vec![name.to_string()];
    owners.extend(idl.mixins_of(name).map(str::to_string));
    for owner in owners {
        let Some(def) = idl.interfaces.get(&owner) else {
            continue;
        };
        let cfg = manifest
            .interfaces
            .get(&owner)
            .or_else(|| manifest.mixins.get(&owner))
            .or_else(|| manifest.namespaces.get(&owner));
        println!(
            "{owner} ({:?}{}){}",
            def.kind,
            def.parent
                .as_ref()
                .map(|p| format!(" : {p}"))
                .unwrap_or_default(),
            if cfg.is_some() { " [in manifest]" } else { "" }
        );
        for m in &def.members {
            let (kind, label, ext) = match m {
                Member::Attribute(a) => (
                    if a.readonly { "ro-attr" } else { "attr" },
                    a.name.clone(),
                    a.ext
                        .0
                        .iter()
                        .map(|e| e.name.clone())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                Member::Operation(o) => (
                    "op",
                    format!(
                        "{}({})",
                        o.name
                            .clone()
                            .unwrap_or_else(|| format!("<{:?}>", o.special)),
                        o.args
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    String::new(),
                ),
                Member::Constructor { args, .. } => (
                    "ctor",
                    format!(
                        "({})",
                        args.iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    String::new(),
                ),
                Member::Const { name, .. } => ("const", name.clone(), String::new()),
                Member::Iterable { key, .. } => (
                    "iterable",
                    if key.is_some() {
                        "pairs".into()
                    } else {
                        "values".into()
                    },
                    String::new(),
                ),
                Member::Stringifier => ("stringifier", String::new(), String::new()),
                Member::Maplike { .. } => ("maplike", String::new(), String::new()),
                Member::Setlike { .. } => ("setlike", String::new(), String::new()),
            };
            let bare = label.split('(').next().unwrap_or("").to_string();
            let mark = match cfg {
                Some(c) if c.members.contains(&bare) => "native",
                Some(c) if c.stubs.contains(&bare) => "stub",
                _ => "",
            };
            println!("  {kind:<11} {label:<48} {ext:<28} {mark}");
        }
    }
    Ok(())
}

pub fn run(args: Args) -> Result<()> {
    let root = workspace_root();
    let (idl, manifest) = load(&root)?;
    if let Some(name) = &args.list {
        return list(&idl, &manifest, name);
    }

    let plan = Planner::new(&idl, &manifest).plan()?;
    let emitter = Emitter::new(&idl, &manifest, &plan);
    let outputs = [
        (
            root.join("crates/catpaw-web/src/generated.rs"),
            rustfmt(&emitter.emit_web())?,
        ),
        (
            root.join("crates/catpaw-bindings-boa/src/generated.rs"),
            rustfmt(&emitter.emit_boa())?,
        ),
    ];

    let mut stale = Vec::new();
    for (path, code) in &outputs {
        let current = std::fs::read_to_string(path).unwrap_or_default();
        if current.replace("\r\n", "\n") == *code {
            continue;
        }
        if args.check {
            stale.push(path.display().to_string());
        } else {
            std::fs::write(path, code).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        }
    }
    if !stale.is_empty() {
        bail!(
            "generated bindings are out of date; run `cargo xtask bindgen`:\n  {}",
            stale.join("\n  ")
        );
    }
    println!(
        "bindgen: {} interfaces, {} mixins with native members, {} namespaces, {} dictionaries, {} enums, {} unions",
        plan.interfaces.len(),
        plan.mixins.len(),
        plan.namespaces.len(),
        plan.dictionaries.len(),
        plan.enums.len(),
        plan.unions.len()
    );
    Ok(())
}
