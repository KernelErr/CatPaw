//! `catpaw setup <host>`: registers `catpaw mcp --stdio` with an agent
//! host. It prints what to run or add; with `--write` it writes the host's
//! configuration itself.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};

#[derive(Clone, Copy, ValueEnum)]
pub enum Host {
    ClaudeCode,
    Codex,
    Cursor,
}

#[derive(Args)]
pub struct SetupArgs {
    /// The agent host.
    #[arg(value_enum)]
    host: Host,
    /// Write the configuration rather than print it: `.mcp.json` here for
    /// Claude Code, `~/.codex/config.toml` for Codex, `~/.cursor/mcp.json`
    /// for Cursor.
    #[arg(long)]
    write: bool,
    /// The name the server goes by in the host.
    #[arg(long, default_value = "catpaw")]
    name: String,
    /// Arguments for `catpaw mcp`, after `--` (`-- --policy strict`).
    #[arg(last = true)]
    extra: Vec<String>,
}

pub fn run(args: SetupArgs) -> Result<()> {
    let exe = std::env::current_exe().context("finding this program")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let command = exe.to_string_lossy().into_owned();
    let mut server = vec!["mcp".to_string(), "--stdio".to_string()];
    server.extend(args.extra.iter().cloned());
    match args.host {
        Host::ClaudeCode => {
            let path = PathBuf::from(".mcp.json");
            if args.write {
                merge_json(&path, &args.name, &command, &server)?;
                eprintln!(
                    "wrote {}; Claude Code offers the server the next time it starts here",
                    path.display()
                );
            } else {
                let words: Vec<String> = std::iter::once(command.as_str())
                    .chain(server.iter().map(String::as_str))
                    .map(shell_quote)
                    .collect();
                println!(
                    "claude mcp add {} -- {}",
                    shell_quote(&args.name),
                    words.join(" ")
                );
                eprintln!(
                    "(or `catpaw setup claude-code --write` for a .mcp.json in this project)"
                );
            }
        }
        Host::Cursor => {
            let path = home()?.join(".cursor").join("mcp.json");
            if args.write {
                merge_json(&path, &args.name, &command, &server)?;
                eprintln!("wrote {}", path.display());
            } else {
                println!("add to {}:", path.display());
                let entry = json!({"mcpServers": {(args.name.clone()): {"command": command, "args": server}}});
                println!("{}", serde_json::to_string_pretty(&entry)?);
            }
        }
        Host::Codex => {
            let path = home()?.join(".codex").join("config.toml");
            let section = toml_section(&args.name, &command, &server);
            if args.write {
                append_toml(&path, &args.name, &section)?;
                eprintln!("wrote {}", path.display());
            } else {
                println!("add to {}:\n\n{section}", path.display());
            }
        }
    }
    Ok(())
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("no home directory (HOME is not set)")
}

/// A word as a POSIX shell reads it.
fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Adds (or replaces) the server in an `mcpServers` JSON file.
fn merge_json(path: &Path, name: &str, command: &str, args: &[String]) -> Result<()> {
    let mut config: Value = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not JSON; not touching it", path.display()))?,
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Some(root) = config.as_object_mut() else {
        bail!("{} is not a JSON object; not touching it", path.display());
    };
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("mcpServers in {} is not an object", path.display()))?;
    servers.insert(name.to_string(), json!({"command": command, "args": args}));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&config)? + "\n")
        .with_context(|| format!("writing {}", path.display()))
}

/// A TOML basic string.
fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        toml_string(name)
    }
}

/// The `[mcp_servers.<name>]` section Codex reads.
fn toml_section(name: &str, command: &str, args: &[String]) -> String {
    let args: Vec<String> = args.iter().map(|a| toml_string(a)).collect();
    format!(
        "[mcp_servers.{}]\ncommand = {}\nargs = [{}]\n",
        toml_key(name),
        toml_string(command),
        args.join(", ")
    )
}

/// Appends the section, unless the file already has one for `name`.
fn append_toml(path: &Path, name: &str, section: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let header = format!("[mcp_servers.{}]", toml_key(name));
    if existing.lines().any(|l| l.trim() == header) {
        bail!(
            "{} already has {header}; edit it there, or remove it and run this again",
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with("\n\n") {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    text.push_str(section);
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_gets_a_toml_section() {
        let args = vec!["mcp".to_string(), "--stdio".to_string()];
        assert_eq!(
            toml_section("catpaw", "C:\\Tools\\catpaw.exe", &args),
            "[mcp_servers.catpaw]\ncommand = \"C:\\\\Tools\\\\catpaw.exe\"\nargs = [\"mcp\", \"--stdio\"]\n"
        );
        let dir = std::env::temp_dir().join(format!("catpaw-setup-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "model = \"o3\"\n").unwrap();
        append_toml(
            &path,
            "catpaw",
            &toml_section("catpaw", "/bin/catpaw", &args),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("model = \"o3\"\n\n[mcp_servers.catpaw]\n"),
            "{text}"
        );
        assert!(append_toml(&path, "catpaw", "").is_err(), "not twice");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_hosts_keep_their_other_servers() {
        let dir = std::env::temp_dir().join(format!("catpaw-setup-json-{}", std::process::id()));
        let path = dir.join("mcp.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"mcpServers": {"other": {"command": "x"}}, "keep": 1}"#,
        )
        .unwrap();
        merge_json(
            &path,
            "catpaw",
            "/bin/catpaw",
            &["mcp".into(), "--stdio".into()],
        )
        .unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            value["mcpServers"]["catpaw"]["args"],
            json!(["mcp", "--stdio"])
        );
        assert_eq!(value["keep"], 1);
        assert_eq!(
            shell_quote("/Applications/My App/catpaw"),
            "'/Applications/My App/catpaw'"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
