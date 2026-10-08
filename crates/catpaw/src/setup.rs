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
    // The path as invoked: a link that a package manager keeps (Homebrew's
    // `bin/`) outlives the versioned file it points to.
    let command = exe.to_string_lossy().into_owned();
    run_in(&args, &command, home().ok(), Path::new("."))
}

/// [`run`] for `command`, with `home` as the user's home directory and
/// `here` as the project directory.
fn run_in(args: &SetupArgs, command: &str, home: Option<PathBuf>, here: &Path) -> Result<()> {
    let home = || home.clone().context("no home directory (HOME is not set)");
    let mut server = vec!["mcp".to_string(), "--stdio".to_string()];
    server.extend(args.extra.iter().cloned());
    match args.host {
        Host::ClaudeCode => {
            let path = here.join(".mcp.json");
            if args.write {
                merge_json(&path, &args.name, command, &server)?;
                eprintln!(
                    "wrote {}; Claude Code offers the server the next time it starts here",
                    path.display()
                );
            } else {
                let words: Vec<String> = std::iter::once(command)
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
                merge_json(&path, &args.name, command, &server)?;
                eprintln!("wrote {}", path.display());
            } else {
                println!("add to {}:", path.display());
                let entry = json!({"mcpServers": {(args.name.clone()): {"command": command, "args": server}}});
                println!("{}", serde_json::to_string_pretty(&entry)?);
            }
        }
        Host::Codex => {
            let path = home()?.join(".codex").join("config.toml");
            let section = toml_section(&args.name, command, &server);
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

/// Adds (or replaces) the server in an `mcpServers` JSON file. The file is
/// edited where the entry goes: the rest keeps its order, spacing and
/// layout.
fn merge_json(path: &Path, name: &str, command: &str, args: &[String]) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let entry = json!({"command": command, "args": args}).to_string();
    let merged = if text.trim().is_empty() {
        let config =
            json!({"mcpServers": {(name.to_string()): {"command": command, "args": args}}});
        serde_json::to_string_pretty(&config)? + "\n"
    } else {
        let config: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not JSON; not touching it", path.display()))?;
        let Some(root) = config.as_object() else {
            bail!("{} is not a JSON object; not touching it", path.display());
        };
        if root.get("mcpServers").is_some_and(|s| !s.is_object()) {
            bail!("mcpServers in {} is not an object", path.display());
        }
        let object = json_scan::span(&text, 0).context("an object")?;
        let key = serde_json::to_string(name)?;
        match json_scan::member(&text, object.clone(), "mcpServers") {
            Some(servers) => match json_scan::member(&text, servers.clone(), name) {
                Some(old) => format!("{}{entry}{}", &text[..old.start], &text[old.end..]),
                None => json_scan::insert(&text, servers, &format!("{key}: {entry}")),
            },
            None => json_scan::insert(
                &text,
                object,
                &format!("\"mcpServers\": {{{key}: {entry}}}"),
            ),
        }
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, merged).with_context(|| format!("writing {}", path.display()))
}

/// Finding the parts of a JSON text that has been checked to be valid.
mod json_scan {
    use std::ops::Range;

    fn skip_space(text: &[u8], mut at: usize) -> usize {
        while at < text.len() && text[at].is_ascii_whitespace() {
            at += 1;
        }
        at
    }

    /// The end of the string starting at `at` (its opening quote).
    fn string_end(text: &[u8], mut at: usize) -> usize {
        at += 1;
        while at < text.len() {
            match text[at] {
                b'\\' => at += 2,
                b'"' => return at + 1,
                _ => at += 1,
            }
        }
        at
    }

    /// The span of the value starting at or after `at`.
    pub fn span(text: &str, at: usize) -> Option<Range<usize>> {
        let bytes = text.as_bytes();
        let start = skip_space(bytes, at);
        let end = match *bytes.get(start)? {
            b'"' => string_end(bytes, start),
            b'{' | b'[' => {
                let mut depth = 0usize;
                let mut i = start;
                loop {
                    match *bytes.get(i)? {
                        b'"' => {
                            i = string_end(bytes, i);
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                break i + 1;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            _ => {
                let mut i = start;
                while i < bytes.len() && !matches!(bytes[i], b',' | b'}' | b']') {
                    i += 1;
                }
                i
            }
        };
        Some(start..end)
    }

    /// The span of the value of member `key` of the object at `object`.
    pub fn member(text: &str, object: Range<usize>, key: &str) -> Option<Range<usize>> {
        let bytes = text.as_bytes();
        let mut at = object.start + 1;
        loop {
            at = skip_space(bytes, at);
            if *bytes.get(at)? != b'"' {
                return None;
            }
            let name_end = string_end(bytes, at);
            let name: String = serde_json::from_str(&text[at..name_end]).ok()?;
            let colon = skip_space(bytes, name_end);
            let value = span(text, colon + 1)?;
            if name == key {
                return Some(value);
            }
            at = skip_space(bytes, value.end);
            if *bytes.get(at)? != b',' {
                return None;
            }
            at += 1;
        }
    }

    /// The text with `member` added as the last member of the object at
    /// `object`.
    pub fn insert(text: &str, object: Range<usize>, member: &str) -> String {
        let close = object.end - 1;
        let inside = text[object.start + 1..close].trim();
        let before = text[..close].trim_end();
        let separator = if inside.is_empty() { "" } else { "," };
        format!("{before}{separator} {member}{}", &text[before.len()..])
    }
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

/// Appends the section, unless the file already has the server (as a
/// table, a dotted key or an inline table: what Codex reads).
fn append_toml(path: &Path, name: &str, section: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let config: toml::Table = existing
        .parse()
        .with_context(|| format!("{} is not TOML; not touching it", path.display()))?;
    let has = config
        .get("mcp_servers")
        .and_then(|servers| servers.get(name))
        .is_some();
    if has {
        bail!(
            "{} already has mcp_servers.{}; edit it there, or remove it and run this again",
            path.display(),
            toml_key(name)
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
        // However the file names the server, it is there already.
        for written in [
            "[mcp_servers.\"catpaw\"]\ncommand = \"x\"\n",
            "[ mcp_servers . catpaw ]\ncommand = \"x\"\n",
            "mcp_servers.catpaw.command = \"x\"\n",
            "[mcp_servers]\ncatpaw = { command = \"x\" }\n",
        ] {
            std::fs::write(&path, written).unwrap();
            assert!(append_toml(&path, "catpaw", "").is_err(), "{written}");
        }
        std::fs::write(&path, "not [toml").unwrap();
        let err = append_toml(&path, "catpaw", "").unwrap_err().to_string();
        assert!(err.contains("not TOML"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn written_where_each_host_reads_it() {
        let dir = std::env::temp_dir().join(format!("catpaw-setup-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let args = |host| SetupArgs {
            host,
            write: true,
            name: "catpaw".into(),
            extra: vec!["--policy".into(), "strict".into()],
        };
        let home = Some(dir.join("home"));
        run_in(&args(Host::Cursor), "/bin/catpaw", home.clone(), &dir).unwrap();
        run_in(&args(Host::Codex), "/bin/catpaw", home.clone(), &dir).unwrap();
        run_in(&args(Host::ClaudeCode), "/bin/catpaw", None, &dir).unwrap();
        let cursor = std::fs::read_to_string(dir.join("home/.cursor/mcp.json")).unwrap();
        assert!(cursor.contains("\"--policy\""), "{cursor}");
        let codex = std::fs::read_to_string(dir.join("home/.codex/config.toml")).unwrap();
        assert!(codex.starts_with("[mcp_servers.catpaw]\n"), "{codex}");
        assert!(dir.join(".mcp.json").exists());
        // Without a home, the hosts that live there cannot be set up.
        assert!(run_in(&args(Host::Cursor), "/bin/catpaw", None, &dir).is_err());
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
        let text = std::fs::read_to_string(&path).unwrap();
        // Edited in place: the other keys keep their order and layout.
        assert!(
            text.starts_with(r#"{"mcpServers": {"other": {"command": "x"}, "catpaw": {"#),
            "{text}"
        );
        assert!(text.ends_with(r#"}}, "keep": 1}"#), "{text}");
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            value["mcpServers"]["catpaw"]["args"],
            json!(["mcp", "--stdio"])
        );
        assert_eq!(value["keep"], 1);
        // Again: the entry is replaced, not added twice.
        merge_json(&path, "catpaw", "/usr/bin/catpaw", &["mcp".into()]).unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["catpaw"]["command"], "/usr/bin/catpaw");
        assert_eq!(value["mcpServers"].as_object().unwrap().len(), 2);
        // A file without servers gets them, after what it has.
        std::fs::write(&path, "{\n  \"z\": [1, \"}\"]\n}\n").unwrap();
        merge_json(&path, "catpaw", "/bin/catpaw", &[]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\n  \"z\": [1, \"}\"], \"mcpServers\""),
            "{text}"
        );
        assert!(serde_json::from_str::<Value>(&text).is_ok(), "{text}");
        assert_eq!(
            shell_quote("/Applications/My App/catpaw"),
            "'/Applications/My App/catpaw'"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
