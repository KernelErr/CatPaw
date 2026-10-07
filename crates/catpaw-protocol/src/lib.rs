//! The agent protocol: the tools CatPaw offers, their parameters, and the
//! wording of their results (ADR 0006).
//!
//! Everything here is static. Tool descriptions and schemas carry no
//! version, host or port, so that what a host puts in front of the model
//! stays byte-identical from one session to the next and prompt caches
//! keep hitting. `protocol.json` next to this crate's manifest is the same
//! data as a file (`cargo xtask protocol` writes it; `--check` verifies it).

pub mod params;
pub mod tools;
pub mod wording;

pub use tools::{TOOLS, ToolDef, tool};

/// What `initialize` tells the host about using the tools (MCP
/// `instructions`; hosts usually put it in the system prompt).
pub const INSTRUCTIONS: &str = "\
CatPaw is a headless browser. Navigate to a URL, read the snapshot that comes back, act on elements by their ref, read the result, repeat. Every action returns its outcome and a fresh snapshot, so a separate snapshot call is rarely needed.

Snapshot lines: `e12 link \"Sign in\"` is an element (ref, role, name), followed by its state in brackets: [value=...], [checked], [disabled], [expanded]; [clickable] marks an element that looks clickable without a role. `text: ...` lines are page text. Indentation is nesting. The first line gives the snapshot id, tab, document, URL and title, and settled=no when the page was still busy.

Refs: an element keeps its ref across snapshots until it leaves the page, and refs are never reused. A ref that went stale gives `error StaleRef` naming the likely replacement. Prefer refs; use css:<selector> for elements no snapshot shows and xy:<x>,<y> as a last resort.

Results start with `ok`, `error <Code>` (nothing happened; the message says why and what to try), `needs_confirmation` (the user must approve; follow the message) or `blocked` (not allowed; do not retry). Lines starting with `!` report consequences: navigations, new tabs, dialogs, console errors.

type replaces a field's value unless append is true. Dialogs (alert, confirm, prompt) are dismissed and reported. Windows a page opens become tabs; switch to them with tabs. To read content rather than act on it, use read: markdown for articles, links for URLs, forms for field values. Screenshots cost many tokens; take them when layout or images matter.
";

/// The protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The tool list and instructions as one JSON document (what
/// `protocol.json` holds).
pub fn protocol_json() -> String {
    let tools: Vec<serde_json::Value> = TOOLS.iter().map(ToolDef::to_json).collect();
    let doc = serde_json::json!({
        "instructions": INSTRUCTIONS,
        "protocolVersions": PROTOCOL_VERSIONS,
        "tools": tools,
    });
    let mut out = serde_json::to_string_pretty(&doc).expect("static JSON serializes");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_parses_and_names_match() {
        for def in TOOLS {
            let schema: serde_json::Value =
                serde_json::from_str(def.schema).unwrap_or_else(|e| panic!("{}: {e}", def.name));
            assert_eq!(schema["type"], "object", "{}", def.name);
            assert_eq!(schema["additionalProperties"], false, "{}", def.name);
            assert!(def.description.contains("\nExample: {"), "{}", def.name);
            let example = def.description.split("\nExample: ").nth(1).unwrap();
            let example: serde_json::Value = serde_json::from_str(example)
                .unwrap_or_else(|e| panic!("{} example: {e}", def.name));
            params::check(def.name, example).unwrap_or_else(|e| panic!("{}: {e}", def.name));
        }
    }

    #[test]
    fn the_tool_list_stays_small() {
        // A fixed cost on every turn of every conversation: keep it lean.
        let size: usize = TOOLS
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.schema.len())
            .sum();
        assert!(size < 9000, "tool definitions grew to {size} bytes");
        assert!(INSTRUCTIONS.split_whitespace().count() < 360);
    }
}
