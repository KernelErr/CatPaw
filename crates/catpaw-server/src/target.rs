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
pub(crate) fn normalize_key(spec: &str) -> String {
    let spec = spec.trim();
    if spec.chars().count() <= 1 {
        return spec.to_string();
    }
    let mut parts: Vec<String> = spec.split('+').map(str::to_string).collect();
    if let Some(name) = parts.last_mut()
        && name.chars().count() > 1
    {
        let canonical = match name.to_ascii_lowercase().as_str() {
            "enter" | "return" => "Enter",
            "tab" => "Tab",
            "esc" | "escape" => "Escape",
            "backspace" => "Backspace",
            "delete" | "del" => "Delete",
            "space" | "spacebar" => "Space",
            "up" | "arrowup" => "ArrowUp",
            "down" | "arrowdown" => "ArrowDown",
            "left" | "arrowleft" => "ArrowLeft",
            "right" | "arrowright" => "ArrowRight",
            "home" => "Home",
            "end" => "End",
            "pageup" => "PageUp",
            "pagedown" => "PageDown",
            _ => "",
        };
        if !canonical.is_empty() {
            *name = canonical.to_string();
        }
    }
    parts.join("+")
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
        assert_eq!(normalize_key("enter"), "Enter");
        assert_eq!(normalize_key("Return"), "Enter");
        assert_eq!(normalize_key("ctrl+a"), "ctrl+a");
        assert_eq!(normalize_key("Shift+tab"), "Shift+Tab");
        assert_eq!(normalize_key("a"), "a");
        assert_eq!(normalize_key("PageDown"), "PageDown");
    }
}
