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

/// The definition of the tool called `name`, optional ones included.
pub fn tool(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().chain(OPTIONAL_TOOLS).find(|t| t.name == name)
}

/// Tools listed only when the server is asked to (`--tools session`).
pub static OPTIONAL_TOOLS: &[ToolDef] = &[ToolDef {
    name: "session",
    title: "Session checkpoints",
    description: "Save the session (cookies, storage, the tabs and where they are) under a name, restore it later, or list what is saved. Restoring loads each tab again; refs start afresh.\nExample: {\"op\":\"save\",\"name\":\"logged-in\"}",
    schema: r#"{"type":"object","properties":{
"op":{"type":"string","enum":["save","restore","list"]},
"name":{"type":"string","description":"For save and restore"}
},"required":["op"],"additionalProperties":false}"#,
    read_only: false,
}];

pub static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "navigate",
        title: "Navigate",
        description: "Open a URL in the current tab (opening a tab when there is none), or go back, forward or reload. Returns the page's snapshot.\nExample: {\"url\":\"https://example.com\"}",
        schema: r#"{"type":"object","properties":{
"url":{"type":"string","description":"https:// is assumed when no scheme is given"},
"go":{"type":"string","enum":["back","forward","reload"],"description":"A history move instead of a URL"},
"snapshot":{"type":"string","enum":["full","diff","none"]},
"confirmation":{"type":"string"}
},"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "snapshot",
        title: "Snapshot",
        description: "Show the current tab as a tree: one line per element worth reading or acting on, each with a ref (e12) that other tools take as target. Actions already return what changed; call this to see the whole page again, with another filter, or one part of it.\nExample: {\"filter\":\"interactive\"}",
        schema: r#"{"type":"object","properties":{
"filter":{"type":"string","enum":["interesting","interactive","all"],"description":"interesting (default): controls, headings, landmarks and text; interactive: controls only; all: every element"},
"root":{"type":"string","description":"Ref of a subtree to show alone (a [collapsed] one, say)"},
"after":{"type":"string","description":"With root: show the items after this ref ([more=… after eN])"},
"maxTokens":{"type":"integer","minimum":200,"description":"Size budget (default 4000)"},
"attrs":{"type":"array","items":{"type":"string","enum":["href","src","description"]},"description":"Extra attributes: link URLs, image sources, descriptions"},
"format":{"type":"string","enum":["compact","aria"],"description":"compact (default): e12 link \"Home\"; aria: - link \"Home\" [ref=e12]. Later results use it too"},
"diff":{"type":"boolean","description":"Only what changed since the last snapshot"}
},"additionalProperties":false}"#,
        read_only: true,
    },
    ToolDef {
        name: "click",
        title: "Click",
        description: "Click an element: scroll it into view, check that nothing covers it, click its centre. Returns the outcome and what changed on the page. count:2 double-clicks; button:\"right\" opens a context menu.\nExample: {\"target\":\"e12\"}",
        schema: r#"{"type":"object","properties":{
"target":{"type":"string","description":"Ref (e12), text:<visible text>, role \"name\", css:<selector> or xy:<x>,<y>"},
"force":{"type":"boolean","description":"Skip the checks (covered, disabled, moving) and click the element itself"},
"button":{"type":"string","enum":["left","middle","right"]},
"count":{"type":"integer","minimum":1,"maximum":3},
"modifiers":{"type":"array","items":{"type":"string","enum":["Control","Shift","Alt","Meta"]}},
"snapshot":{"type":"string","enum":["diff","full","none"]},
"dialog":{"type":"string","enum":["accept","dismiss"]},
"promptText":{"type":"string"},
"confirmation":{"type":"string"}
},"required":["target"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "type",
        title: "Type",
        description: "Type into a text field or editable element, replacing its value unless append is true; submit presses Enter afterwards. Without target, types into the focused element.\nExample: {\"target\":\"e5\",\"text\":\"catpaw\",\"submit\":true}",
        schema: r#"{"type":"object","properties":{
"target":{"type":"string","description":"Ref (e12), text:<visible text>, role \"name\" or css:<selector>"},
"text":{"type":"string"},
"append":{"type":"boolean","description":"Keep the current value and add to it"},
"submit":{"type":"boolean","description":"Press Enter afterwards"},
"snapshot":{"type":"string","enum":["diff","full","none"]},
"dialog":{"type":"string","enum":["accept","dismiss"]},
"promptText":{"type":"string"},
"confirmation":{"type":"string"}
},"required":["text"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "fill",
        title: "Fill form",
        description: "Set several fields at once: text for a text field or editable element (replacing it), true or false for a checkbox or radio button, an option label (or a list) for a select; submit presses Enter in the last field.\nExample: {\"fields\":[{\"target\":\"e3\",\"value\":\"Ada\"},{\"target\":\"e7\",\"value\":true}]}",
        schema: r#"{"type":"object","properties":{
"fields":{"type":"array","minItems":1,"maxItems":50,"items":{"type":"object","properties":{
"target":{"type":"string","description":"Ref (e12), text:<visible text>, role \"name\" or css:<selector>"},
"value":{"anyOf":[{"type":"string"},{"type":"boolean"},{"type":"array","items":{"type":"string"}}]}
},"required":["target","value"],"additionalProperties":false}},
"submit":{"type":"boolean"},
"snapshot":{"type":"string","enum":["diff","full","none"]},
"dialog":{"type":"string","enum":["accept","dismiss"]},
"promptText":{"type":"string"},
"confirmation":{"type":"string"}
},"required":["fields"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "press",
        title: "Press key",
        description: "Press a key or chord on the focused element, or on target after focusing it: Enter, Tab, Escape, ArrowDown, Backspace, Control+a, Shift+Tab.\nExample: {\"key\":\"Enter\"}",
        schema: r#"{"type":"object","properties":{
"key":{"type":"string"},
"target":{"type":"string","description":"Ref (e12), text:<visible text>, role \"name\" or css:<selector>"},
"repeat":{"type":"integer","minimum":1,"maximum":50},
"snapshot":{"type":"string","enum":["diff","full","none"]},
"dialog":{"type":"string","enum":["accept","dismiss"]},
"promptText":{"type":"string"},
"confirmation":{"type":"string"}
},"required":["key"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "select",
        title: "Select option",
        description: "Choose an option of a <select> by its visible label (or value); an array chooses several in a multiple select. When nothing matches, the error lists the options.\nExample: {\"target\":\"e8\",\"option\":\"Price (low to high)\"}",
        schema: r#"{"type":"object","properties":{
"target":{"type":"string","description":"Ref (e12), text:<visible text>, role \"name\" or css:<selector>"},
"option":{"anyOf":[{"type":"string"},{"type":"array","items":{"type":"string"}}]},
"snapshot":{"type":"string","enum":["diff","full","none"]},
"dialog":{"type":"string","enum":["accept","dismiss"]},
"promptText":{"type":"string"},
"confirmation":{"type":"string"}
},"required":["target","option"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "act",
        title: "Other action",
        description: "Less common element actions: hover, check, uncheck, focus, clear (empty a field), scroll (target into view, or the page by dy pixels; one screen down by default), upload (choose local files in a file input), drag (target onto to).\nExample: {\"kind\":\"check\",\"target\":\"e14\"}",
        schema: r#"{"type":"object","properties":{
"kind":{"type":"string","enum":["hover","check","uncheck","focus","clear","scroll","upload","drag"]},
"target":{"type":"string","description":"Ref (e12), text:<visible text>, role \"name\" or css:<selector>"},
"dy":{"type":"number","description":"Page scroll in pixels; negative scrolls up"},
"files":{"type":"array","items":{"type":"string"},"description":"For upload: paths of local files"},
"to":{"type":"string","description":"For drag: where to drop it (a target)"},
"snapshot":{"type":"string","enum":["diff","full","none"]},
"dialog":{"type":"string","enum":["accept","dismiss"]},
"promptText":{"type":"string"},
"confirmation":{"type":"string"}
},"required":["kind"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "read",
        title: "Read",
        description: "Read the current tab as text rather than a tree. markdown: the content, links as [text](ref:e12); text: plain text; links: ref, text and URL per line; forms: fields with refs, values and options; tables: Markdown tables with row refs; find: where query (text or /regex/) occurs, with refs; html: the markup. Long output stops at maxTokens and says which offset continues it.\nExample: {\"view\":\"markdown\"}",
        schema: r#"{"type":"object","properties":{
"view":{"type":"string","enum":["markdown","text","links","forms","tables","find","html"]},
"query":{"type":"string","description":"For find"},
"root":{"type":"string","description":"Ref of the part to read"},
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
"target":{"type":"string","description":"Ref (e12), text:<visible text>, role \"name\" or css:<selector>"},
"confirmation":{"type":"string"}
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
    ToolDef {
        name: "wait",
        title: "Wait",
        description: "Let the page run until it settles, a text appears or goes, an element is visible, the URL contains a string, or ms milliseconds pass; or until the user gives back a tab handed over with handoff. Time the page spends only waiting on timers passes at once. Returns what changed.\nExample: {\"for\":\"text\",\"text\":\"Order placed\"}",
        schema: r#"{"type":"object","properties":{
"for":{"type":"string","enum":["settled","text","gone","visible","url","time","handoff"]},
"text":{"type":"string","description":"For text and gone"},
"target":{"type":"string","description":"For visible and gone: ref or css:<selector>"},
"url":{"type":"string","description":"For url: part of the URL"},
"ms":{"type":"integer","minimum":1,"description":"For time"},
"timeoutMs":{"type":"integer","minimum":1,"description":"Default 10000; for handoff 600000"},
"snapshot":{"type":"string","enum":["diff","full","none"]}
},"required":["for"],"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "handoff",
        title: "Hand over to the user",
        description: "Hand the current tab to the user, on a page they open in their own browser: to log in, pass a check meant for a person, or do anything you should not do or see. Then wait({\"for\":\"handoff\"}) until they give it back; you see the page, not what they typed.\nExample: {\"reason\":\"Log in to your account\"}",
        schema: r#"{"type":"object","properties":{
"reason":{"type":"string","description":"What to ask the user to do"}
},"additionalProperties":false}"#,
        read_only: false,
    },
    ToolDef {
        name: "logs",
        title: "Logs",
        description: "The current tab's console messages, requests, or events (navigations, new tabs, dialogs), oldest first.\nExample: {\"kind\":\"console\",\"level\":\"error\"}",
        schema: r#"{"type":"object","properties":{
"kind":{"type":"string","enum":["console","network","events"]},
"level":{"type":"string","enum":["error","warn","info","log","debug"],"description":"console: this level and above"},
"match":{"type":"string","description":"Only entries containing this"},
"since":{"type":"string","description":"Only entries after snapshot sN"},
"limit":{"type":"integer","minimum":1,"maximum":200}
},"required":["kind"],"additionalProperties":false}"#,
        read_only: true,
    },
];
