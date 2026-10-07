//! The tools, as `tools/list` presents them.
//!
//! Schemas are written by hand rather than derived: every word in them is
//! read by the model on every turn, so each one is chosen. Each
//! description ends with one example call; a test checks that the example
//! parses with the tool's parameter type.

/// One tool: its name, description (ending in an example) and JSON Schema
/// for its arguments.
#[derive(Debug, Clone, Copy)]
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub schema: &'static str,
    /// The tool only looks at the page (MCP `readOnlyHint`).
    pub read_only: bool,
}

impl ToolDef {
    /// The tool as an entry of `tools/list`.
    pub fn to_json(&self) -> serde_json::Value {
        let schema: serde_json::Value =
            serde_json::from_str(self.schema).expect("tool schemas are valid JSON");
        let mut tool = serde_json::json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": schema,
        });
        if self.read_only {
            tool["annotations"] = serde_json::json!({ "readOnlyHint": true });
        }
        tool
    }
}

/// The definition of the tool called `name`.
pub fn tool(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

pub static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "navigate",
        title: "Navigate",
        description: "Open a URL in the current tab (opening a tab when there is none), or go back, forward or reload. Returns the page's snapshot.\nExample: {\"url\":\"https://example.com\"}",
        schema: r#"{"type":"object","properties":{
"url":{"type":"string","description":"https:// is assumed when no scheme is given"},
"go":{"type":"string","enum":["back","forward","reload"],"description":"A history move instead of a URL"}
},"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "snapshot",
        title: "Snapshot",
        description: "Show the current tab as a tree: one line per element worth reading or acting on, each with a ref (e12) that other tools take as target. Actions already return a fresh snapshot; call this to change the filter, focus on a subtree, or look again later.\nExample: {\"filter\":\"interactive\"}",
        schema: r#"{"type":"object","properties":{
"filter":{"type":"string","enum":["interesting","interactive","all"],"description":"interesting (default): controls, headings, landmarks and text; interactive: controls only; all: every element"},
"root":{"type":"string","description":"Ref of a subtree to show alone"},
"maxTokens":{"type":"integer","minimum":200,"description":"Size budget (default 4000)"},
"attrs":{"type":"array","items":{"type":"string","enum":["href","src","description"]},"description":"Extra attributes: link URLs, image sources, descriptions"},
"format":{"type":"string","enum":["compact","aria"],"description":"compact (default): e12 link \"Home\"; aria: - link \"Home\" [ref=e12]. Later results use it too"}
},"additionalProperties":false}"#,
        read_only: true,
    },
    ToolDef {
        name: "click",
        title: "Click",
        description: "Click an element: scroll it into view, check that nothing covers it, click its centre. Returns the outcome and a fresh snapshot.\nExample: {\"target\":\"e12\"}",
        schema: r#"{"type":"object","properties":{
"target":{"type":"string","description":"Ref (e12), css:<selector> or xy:<x>,<y>"}
},"required":["target"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "type",
        title: "Type",
        description: "Type into a text field or editable element, replacing its value unless append is true; submit presses Enter afterwards. Without target, types into the focused element.\nExample: {\"target\":\"e5\",\"text\":\"catpaw\",\"submit\":true}",
        schema: r#"{"type":"object","properties":{
"target":{"type":"string","description":"Ref (e12) or css:<selector>"},
"text":{"type":"string"},
"append":{"type":"boolean","description":"Keep the current value and add to it"},
"submit":{"type":"boolean","description":"Press Enter afterwards"}
},"required":["text"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "press",
        title: "Press key",
        description: "Press a key or chord on the focused element, or on target after focusing it: Enter, Tab, Escape, ArrowDown, Backspace, Control+a, Shift+Tab.\nExample: {\"key\":\"Enter\"}",
        schema: r#"{"type":"object","properties":{
"key":{"type":"string"},
"target":{"type":"string","description":"Ref (e12) or css:<selector>"},
"repeat":{"type":"integer","minimum":1,"maximum":50}
},"required":["key"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "select",
        title: "Select option",
        description: "Choose an option of a <select> by its visible label (or value); an array chooses several in a multiple select. When nothing matches, the error lists the options.\nExample: {\"target\":\"e8\",\"option\":\"Price (low to high)\"}",
        schema: r#"{"type":"object","properties":{
"target":{"type":"string","description":"Ref (e12) or css:<selector>"},
"option":{"anyOf":[{"type":"string"},{"type":"array","items":{"type":"string"}}]}
},"required":["target","option"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "act",
        title: "Other action",
        description: "Less common element actions: hover, check, uncheck, focus, clear (empty a field), scroll (target into view, or the page by dy pixels; one screen down by default).\nExample: {\"kind\":\"check\",\"target\":\"e14\"}",
        schema: r#"{"type":"object","properties":{
"kind":{"type":"string","enum":["hover","check","uncheck","focus","clear","scroll"]},
"target":{"type":"string","description":"Ref (e12) or css:<selector>"},
"dy":{"type":"number","description":"Page scroll in pixels; negative scrolls up"}
},"required":["kind"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "read",
        title: "Read",
        description: "Read the current tab as text rather than a tree. markdown: the content, links as [text](ref:e12); text: plain text; links: ref, text and URL per line; forms: each form's fields with refs, values and options. Long output stops at maxTokens and says which offset continues it.\nExample: {\"view\":\"markdown\"}",
        schema: r#"{"type":"object","properties":{
"view":{"type":"string","enum":["markdown","text","links","forms"]},
"main":{"type":"boolean","description":"markdown and text: only the main content, when the page marks it"},
"offset":{"type":"integer","minimum":0},
"maxTokens":{"type":"integer","minimum":200,"description":"Size budget (default 6000)"}
},"required":["view"],"additionalProperties":false}"#,
        read_only: true,
    },
    ToolDef {
        name: "screenshot",
        title: "Screenshot",
        description: "A PNG of the current tab's viewport, or of the whole page. Costs far more tokens than a snapshot; take one when layout, images or canvas matter.\nExample: {}",
        schema: r#"{"type":"object","properties":{
"fullPage":{"type":"boolean"}
},"additionalProperties":false}"#,
        read_only: true,
    },
    ToolDef {
        name: "evaluate",
        title: "Evaluate JavaScript",
        description: "Run JavaScript in the page and return the result as a console shows it; promises are awaited. The script is an expression, or statements that return. el is the target's element; $ref(\"e12\") gives any ref's element.\nExample: {\"script\":\"el.value\",\"target\":\"e5\"}",
        schema: r#"{"type":"object","properties":{
"script":{"type":"string"},
"target":{"type":"string","description":"Ref (e12) or css:<selector>"}
},"required":["script"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "tabs",
        title: "Tabs",
        description: "List, switch to, open or close tabs. A window a page opens becomes a tab, and the action that opened it names it.\nExample: {\"op\":\"switch\",\"tab\":\"t2\"}",
        schema: r#"{"type":"object","properties":{
"op":{"type":"string","enum":["list","switch","open","close"]},
"tab":{"type":"string","description":"Tab id; close defaults to the current tab"},
"url":{"type":"string","description":"For open"}
},"required":["op"],"additionalProperties":false}"#,
        read_only: false,
    },
];
