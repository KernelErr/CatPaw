//! Tool arguments. Unknown fields are refused, so that a misspelt option
//! gets an error naming the right ones instead of being ignored.

use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Navigate {
    pub url: Option<String>,
    pub go: Option<Go>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Go {
    Back,
    Forward,
    Reload,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Snapshot {
    pub filter: Option<Filter>,
    pub root: Option<String>,
    pub max_tokens: Option<u32>,
    pub attrs: Option<Vec<Attr>>,
    pub format: Option<Format>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    Interesting,
    Interactive,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Attr {
    Href,
    Src,
    Description,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Compact,
    Aria,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Click {
    pub target: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Type {
    pub target: Option<String>,
    pub text: String,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub submit: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Press {
    pub key: String,
    pub target: Option<String>,
    pub repeat: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Select {
    pub target: String,
    pub option: OneOrMany,
}

/// A string, or an array of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s.clone()],
            OneOrMany::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Act {
    pub kind: ActKind,
    pub target: Option<String>,
    pub dy: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActKind {
    Hover,
    Check,
    Uncheck,
    Focus,
    Clear,
    Scroll,
}

impl ActKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActKind::Hover => "hover",
            ActKind::Check => "check",
            ActKind::Uncheck => "uncheck",
            ActKind::Focus => "focus",
            ActKind::Clear => "clear",
            ActKind::Scroll => "scroll",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Read {
    pub view: ReadView,
    #[serde(default)]
    pub main: bool,
    pub offset: Option<usize>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadView {
    Markdown,
    Text,
    Links,
    Forms,
}

impl ReadView {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadView::Markdown => "markdown",
            ReadView::Text => "text",
            ReadView::Links => "links",
            ReadView::Forms => "forms",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Screenshot {
    #[serde(default)]
    pub full_page: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Evaluate {
    pub script: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Tabs {
    pub op: TabsOp,
    pub tab: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabsOp {
    List,
    Switch,
    Open,
    Close,
}

/// A tool call with its arguments parsed.
#[derive(Debug, Clone)]
pub enum Call {
    Navigate(Navigate),
    Snapshot(Snapshot),
    Click(Click),
    Type(Type),
    Press(Press),
    Select(Select),
    Act(Act),
    Read(Read),
    Screenshot(Screenshot),
    Evaluate(Evaluate),
    Tabs(Tabs),
}

fn args<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, String> {
    let value = match value {
        serde_json::Value::Null => serde_json::Value::Object(Default::default()),
        other => other,
    };
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Parses the arguments of tool `name`. `Err` says what is wrong with them
/// (or that there is no such tool).
pub fn parse(name: &str, value: serde_json::Value) -> Result<Call, String> {
    Ok(match name {
        "navigate" => Call::Navigate(args(value)?),
        "snapshot" => Call::Snapshot(args(value)?),
        "click" => Call::Click(args(value)?),
        "type" => Call::Type(args(value)?),
        "press" => Call::Press(args(value)?),
        "select" => Call::Select(args(value)?),
        "act" => Call::Act(args(value)?),
        "read" => Call::Read(args(value)?),
        "screenshot" => Call::Screenshot(args(value)?),
        "evaluate" => Call::Evaluate(args(value)?),
        "tabs" => Call::Tabs(args(value)?),
        other => return Err(format!("no tool is called {other:?}")),
    })
}

/// Whether `value` is valid as the arguments of tool `name`.
pub fn check(name: &str, value: serde_json::Value) -> Result<(), String> {
    parse(name, value).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_fields_are_named() {
        let err = parse("click", json!({"target": "e1", "ref": "e1"})).unwrap_err();
        assert!(err.contains("unknown field `ref`"), "{err}");
        assert!(err.contains("`target`"), "{err}");
    }

    #[test]
    fn missing_arguments_are_an_empty_object() {
        assert!(matches!(
            parse("screenshot", serde_json::Value::Null),
            Ok(Call::Screenshot(Screenshot { full_page: false }))
        ));
        let err = parse("click", serde_json::Value::Null).unwrap_err();
        assert!(err.contains("missing field `target`"), "{err}");
    }

    #[test]
    fn select_takes_one_option_or_several() {
        let Ok(Call::Select(one)) = parse("select", json!({"target": "e1", "option": "A"})) else {
            panic!()
        };
        assert_eq!(one.option.to_vec(), ["A"]);
        let Ok(Call::Select(many)) = parse("select", json!({"target": "e1", "option": ["A", "B"]}))
        else {
            panic!()
        };
        assert_eq!(many.option.to_vec(), ["A", "B"]);
    }
}
