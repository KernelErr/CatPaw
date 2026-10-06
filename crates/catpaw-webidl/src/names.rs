//! Identifier conversions between IDL and Rust naming.

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "final", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
    "move", "mut", "override", "pub", "ref", "return", "self", "static", "struct", "super",
    "trait", "true", "try", "type", "unsafe", "use", "virtual", "where", "while", "yield",
    "abstract", "become", "do", "macro", "priv", "typeof", "unsized", "gen",
];

/// `parentNode` → `parent_node`, `innerHTML` → `inner_html`,
/// `getElementsByTagNameNS` → `get_elements_by_tag_name_ns`, `URL` → `url`.
pub fn snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c == '-' || c == ' ' {
            out.push('_');
            continue;
        }
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if i > 0 && (prev_lower || (prev_upper && next_lower)) && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if RUST_KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out
}

/// `"no-referrer"` → `NoReferrer`, `""` → `Empty`, `"2d"` → `_2d`.
pub fn variant(value: &str) -> String {
    if value.is_empty() {
        return "Empty".to_string();
    }
    let mut out = String::new();
    let mut upper = true;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() {
            if upper {
                out.push(c.to_ascii_uppercase());
                upper = false;
            } else {
                out.push(c);
            }
        } else {
            upper = true;
        }
    }
    if out.is_empty() {
        out.push_str("Value");
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// A Rust string literal for `s`.
pub fn lit(s: &str) -> String {
    format!("{s:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case() {
        assert_eq!(snake("parentNode"), "parent_node");
        assert_eq!(snake("innerHTML"), "inner_html");
        assert_eq!(
            snake("getElementsByTagNameNS"),
            "get_elements_by_tag_name_ns"
        );
        assert_eq!(snake("URL"), "url");
        assert_eq!(snake("baseURI"), "base_uri");
        assert_eq!(snake("type"), "type_");
        assert_eq!(snake("HTMLElement"), "html_element");
        assert_eq!(snake("createCDATASection"), "create_cdata_section");
        assert_eq!(snake("x1"), "x1");
        assert_eq!(snake("toJSON"), "to_json");
    }

    #[test]
    fn variants() {
        assert_eq!(variant("open"), "Open");
        assert_eq!(variant("no-referrer"), "NoReferrer");
        assert_eq!(variant(""), "Empty");
        assert_eq!(variant("2d"), "_2d");
        assert_eq!(variant("same-origin"), "SameOrigin");
    }
}
