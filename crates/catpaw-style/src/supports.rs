//! `CSS.supports()` and `CSS.escape()`.

use std::borrow::Cow;

use cssparser::Parser;
use selectors::matching::QuirksMode;
use style::custom_properties::AttrTaint;
use style::parser::ParserContext;
use style::servo_arc::Arc;
use style::stylesheets::supports_rule::parse_condition_or_declaration;
use style::stylesheets::{CssRuleType, Namespaces, Origin, UrlExtraData};
use style_traits::ParsingMode;
use url::Url;

use crate::engine::set_prefs;

/// Whether the engine supports a `@supports` condition, or a lone
/// `property: value` declaration.
pub fn supports(condition: &str) -> bool {
    set_prefs();
    let url_data = UrlExtraData(Arc::new(Url::parse("about:blank").expect("valid URL")));
    let namespaces = Namespaces::default();
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(CssRuleType::Style),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Borrowed(&namespaces),
        None,
        None,
        AttrTaint::default(),
    );
    let mut parser = Parser::new(condition);
    let Ok(condition) = parse_condition_or_declaration(&mut parser) else {
        return false;
    };
    if parser.expect_exhausted().is_err() {
        return false;
    }
    condition.eval(&context)
}

/// Whether `property: value` is a declaration the engine understands.
pub fn supports_declaration(property: &str, value: &str) -> bool {
    if property.contains([':', ';', '{', '}']) {
        return false;
    }
    supports(&format!("{property}:{value}"))
}

/// <https://drafts.csswg.org/cssom/#serialize-an-identifier>
pub fn escape(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len());
    let chars: Vec<char> = ident.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let code = c as u32;
        let control = (0x1..=0x1f).contains(&code) || code == 0x7f;
        let leading_digit = c.is_ascii_digit() && (i == 0 || (i == 1 && chars[0] == '-'));
        if code == 0 {
            out.push('\u{FFFD}');
        } else if control || leading_digit {
            out.push_str(&format!("\\{code:x} "));
        } else if i == 0 && c == '-' && chars.len() == 1 {
            out.push('\\');
            out.push(c);
        } else if code >= 0x80 || c == '-' || c == '_' || c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_what_is_supported() {
        assert!(supports_declaration("display", "flex"));
        assert!(supports_declaration("DISPLAY", "grid"));
        assert!(!supports_declaration("display", "nonsense"));
        assert!(!supports_declaration("made-up-property", "1"));
        assert!(supports_declaration("--custom", "anything"));
        assert!(supports("(display: flex) and (not (display: nope))"));
        assert!(!supports("(display: nope)"));
        assert!(supports("display: block"));
        assert!(!supports("selector(:nonsense-pseudo)"));
        assert!(!supports(""));
    }

    #[test]
    fn escapes_identifiers() {
        assert_eq!(escape("a.b c"), "a\\.b\\ c");
        assert_eq!(escape("1st"), "\\31 st");
        assert_eq!(escape("-1"), "-\\31 ");
        assert_eq!(escape("-"), "\\-");
        assert_eq!(escape("\0x"), "\u{FFFD}x");
        assert_eq!(escape("\u{1}"), "\\1 ");
        assert_eq!(escape("ünïcode-ok_9"), "ünïcode-ok_9");
    }
}
