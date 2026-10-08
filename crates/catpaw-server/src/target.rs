//! The `target` argument: a ref, a CSS selector or a point.

use catpaw_agent::RefTable;
use catpaw_protocol::wording::advice;

use crate::output::Failure;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    /// `e12`.
    Ref(String),
    /// `css:<selector>`.
    Css(String),
    /// `xy:<x>,<y>`, viewport CSS pixels.
    Point(f32, f32),
    /// `text:<visible text>`: the element showing that text.
    Text(String),
    /// `role "name"`: a snapshot line without its ref.
    Named(String, String),
}

/// Parses a target. A whole snapshot line (`e12 link "Home"`) or the
/// aria form (`[ref=e12]`) is taken for its ref: models copy those.
pub(crate) fn parse(text: &str) -> Result<Target, Failure> {
    let t = text.trim();
    if let Some(css) = t.strip_prefix("css:") {
        let css = css.trim();
        if css.is_empty() {
            return Err(Failure::bad_argument("css: needs a selector").with(advice::TARGET_SYNTAX));
        }
        return Ok(Target::Css(css.to_string()));
    }
    if let Some(xy) = t.strip_prefix("xy:") {
        let point = xy.split_once(',').and_then(|(x, y)| {
            let x: f32 = x.trim().parse().ok()?;
            let y: f32 = y.trim().parse().ok()?;
            (x.is_finite() && y.is_finite()).then_some((x, y))
        });
        return match point {
            Some((x, y)) => Ok(Target::Point(x, y)),
            None => {
                Err(Failure::bad_argument(format!("{t:?} is not a point"))
                    .with(advice::TARGET_SYNTAX))
            }
        };
    }
    if let Some(text) = t.strip_prefix("text:") {
        let text = text.trim();
        if text.is_empty() {
            return Err(Failure::bad_argument("text: needs a text").with(advice::TARGET_SYNTAX));
        }
        return Ok(Target::Text(text.to_string()));
    }
    // `button "Sign in"`: a role, then a quoted name.
    if let Some((role, rest)) = t.split_once(' ')
        && !role.is_empty()
        && role.bytes().all(|b| b.is_ascii_lowercase())
        && let Some(name) = rest.trim().strip_prefix('"')
        && let Some(name) = name.strip_suffix('"')
    {
        return Ok(Target::Named(role.to_string(), name.replace("\\\"", "\"")));
    }
    let first = t.split_whitespace().next().unwrap_or("");
    let first = first
        .trim_start_matches(['[', '-', ' '])
        .trim_end_matches([']', ':']);
    let first = first.strip_prefix("ref=").unwrap_or(first);
    if first.starts_with('e') && RefTable::parse(first).is_some() {
        return Ok(Target::Ref(first.to_string()));
    }
    if let Some(r) = t.find("[ref=e") {
        let rest = &t[r + 5..];
        let end = rest.find(']').unwrap_or(rest.len());
        if RefTable::parse(&rest[..end]).is_some() {
            return Ok(Target::Ref(rest[..end].to_string()));
        }
    }
    Err(Failure::bad_argument(format!("{t:?} is not a target")).with(advice::TARGET_SYNTAX))
}

/// Canonical key names for the aliases models use (`enter`, `Return`,
/// `esc`, `Up`); chords keep their modifiers.
/// The keys `press` knows by name, besides single characters.
const KEY_NAMES: &str = "Enter, Tab, Escape, Backspace, Delete, Space, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Home, End, PageUp, PageDown, F1-F12";

/// A key as `press` takes it, names and modifiers spelled the standard way
/// (`ctrl+enter` is `Control+Enter`); an error names what is not a key.
pub(crate) fn normalize_key(spec: &str) -> Result<String, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("key is empty".to_string());
    }
    if spec.chars().count() == 1 {
        return Ok(spec.to_string());
    }
    // `Control++` is Control with the plus key.
    let (mods, name) = match spec.strip_suffix("++") {
        Some(mods) => (mods, "+"),
        None => match spec.rsplit_once('+') {
            Some((mods, name)) => (mods, name),
            None => ("", spec),
        },
    };
    let mut out = Vec::new();
    for m in mods
        .split('+')
        .filter(|m| !mods.is_empty() || !m.is_empty())
    {
        let canonical = match m.trim().to_ascii_lowercase().as_str() {
            "control" | "ctrl" => "Control",
            "shift" => "Shift",
            "alt" | "option" => "Alt",
            "meta" | "cmd" | "command" | "super" | "win" => "Meta",
            _ => {
                return Err(format!(
                    "{m:?} in {spec:?} is not a modifier (Control, Shift, Alt, Meta)"
                ));
            }
        };
        out.push(canonical.to_string());
    }
    let name = name.trim();
    let canonical = if name.chars().count() == 1 {
        name.to_string()
    } else {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "enter" | "return" => "Enter".to_string(),
            "tab" => "Tab".to_string(),
            "esc" | "escape" => "Escape".to_string(),
            "backspace" => "Backspace".to_string(),
            "delete" | "del" => "Delete".to_string(),
            "space" | "spacebar" => "Space".to_string(),
            "up" | "arrowup" => "ArrowUp".to_string(),
            "down" | "arrowdown" => "ArrowDown".to_string(),
            "left" | "arrowleft" => "ArrowLeft".to_string(),
            "right" | "arrowright" => "ArrowRight".to_string(),
            "home" => "Home".to_string(),
            "end" => "End".to_string(),
            "pageup" => "PageUp".to_string(),
            "pagedown" => "PageDown".to_string(),
            "insert" => "Insert".to_string(),
            "control" | "ctrl" => "Control".to_string(),
            "shift" => "Shift".to_string(),
            "alt" => "Alt".to_string(),
            "meta" => "Meta".to_string(),
            f if f.starts_with('f')
                && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) =>
            {
                f.to_ascii_uppercase()
            }
            _ => {
                return Err(format!(
                    "{name:?} is not a key: use one character or {KEY_NAMES}, with Control+, Shift+, Alt+ or Meta+ in front"
                ));
            }
        }
    };
    out.push(canonical);
    Ok(out.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse() {
        assert_eq!(parse("e12").unwrap(), Target::Ref("e12".into()));
        assert_eq!(
            parse(" e12 link \"Home\"").unwrap(),
            Target::Ref("e12".into())
        );
        assert_eq!(parse("[ref=e7]").unwrap(), Target::Ref("e7".into()));
        assert_eq!(parse("ref=e7").unwrap(), Target::Ref("e7".into()));
        assert_eq!(
            parse("- link \"Home\" [ref=e9]").unwrap(),
            Target::Ref("e9".into())
        );
        assert_eq!(parse("css:#a > b").unwrap(), Target::Css("#a > b".into()));
        assert_eq!(parse("xy:10, 20.5").unwrap(), Target::Point(10.0, 20.5));
        assert!(parse("Sign in").is_err());
        assert_eq!(
            parse("text: Sign in").unwrap(),
            Target::Text("Sign in".into())
        );
        assert_eq!(
            parse("button \"Sign in\"").unwrap(),
            Target::Named("button".into(), "Sign in".into())
        );
        assert_eq!(parse("e3 button \"Go\"").unwrap(), Target::Ref("e3".into()));
        assert!(parse("xy:1").is_err());
        assert!(parse("css:").is_err());
    }

    #[test]
    fn keys_normalize() {
        let key = |spec: &str| normalize_key(spec).unwrap();
        assert_eq!(key("enter"), "Enter");
        assert_eq!(key("Return"), "Enter");
        assert_eq!(key("ctrl+a"), "Control+a");
        assert_eq!(key("cmd+A"), "Meta+A");
        assert_eq!(key("Shift+tab"), "Shift+Tab");
        assert_eq!(key("a"), "a");
        assert_eq!(key("+"), "+");
        assert_eq!(key("Control++"), "Control++");
        assert_eq!(key("PageDown"), "PageDown");
        assert_eq!(key("f5"), "F5");
        let wrong = normalize_key("Enterr").unwrap_err();
        assert!(wrong.starts_with("\"Enterr\" is not a key"), "{wrong}");
        let wrong = normalize_key("Hyper+a").unwrap_err();
        assert!(wrong.contains("is not a modifier"), "{wrong}");
        assert!(normalize_key("  ").is_err());
    }
}
