//! The CSSOM's view of a style sheet: its top-level rules, each parsed
//! and serialised back, which is what `cssRules[i].cssText` reports and
//! what the engine is given once script has edited a sheet.

use selectors::matching::QuirksMode;
use style::media_queries::MediaList;
use style::servo_arc::Arc;
use style::shared_lock::{SharedRwLock, ToCssWithGuard};
use style::stylesheets::{
    AllowImportRules, CssRule, CssRuleType, Origin, Stylesheet, UrlExtraData,
};
use url::Url;

use crate::engine::set_prefs;

/// A top-level rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// `CSSRule.type`: the historical constants, 0 for newer kinds of rule.
    pub kind: u16,
    pub css_text: String,
    /// The selector list of a style rule.
    pub selector_text: Option<String>,
}

/// `CSSRule.STYLE_RULE`.
pub const STYLE_RULE: u16 = 1;
/// `CSSRule.IMPORT_RULE`.
pub const IMPORT_RULE: u16 = 3;

fn rule_type(rule: &CssRule) -> u16 {
    match rule.rule_type() {
        CssRuleType::Style => 1,
        CssRuleType::Import => 3,
        CssRuleType::Media => 4,
        CssRuleType::FontFace => 5,
        CssRuleType::Page => 6,
        CssRuleType::Keyframes => 7,
        CssRuleType::Keyframe => 8,
        CssRuleType::Margin => 9,
        CssRuleType::Namespace => 10,
        CssRuleType::CounterStyle => 11,
        CssRuleType::Supports => 12,
        CssRuleType::Document => 13,
        CssRuleType::FontFeatureValues => 14,
        _ => 0,
    }
}

fn parse(css: &str, base: Option<&Url>, allow_imports: AllowImportRules) -> Vec<Rule> {
    set_prefs();
    let url = base
        .cloned()
        .unwrap_or_else(|| Url::parse("about:blank").expect("valid URL"));
    let lock = SharedRwLock::new();
    let sheet = Stylesheet::from_str(
        css,
        UrlExtraData(Arc::new(url)),
        Origin::Author,
        Arc::new(lock.wrap(MediaList::empty())),
        lock.clone(),
        None,
        None,
        QuirksMode::NoQuirks,
        allow_imports,
    );
    let guard = lock.read();
    sheet
        .contents
        .read_with(&guard)
        .rules
        .read_with(&guard)
        .0
        .iter()
        .map(|rule| Rule {
            kind: rule_type(rule),
            css_text: rule.to_css_string(&guard),
            selector_text: match rule {
                CssRule::Style(style) => Some(cssparser::ToCss::to_css_string(
                    &style.read_with(&guard).selectors,
                )),
                _ => None,
            },
        })
        .collect()
}

/// The top-level rules of a sheet, in order. `allow_imports` is false for
/// constructed sheets, whose `@import` rules are dropped.
pub fn parse_rules(css: &str, base: Option<&Url>, allow_imports: bool) -> Vec<Rule> {
    let allow = if allow_imports {
        AllowImportRules::Yes
    } else {
        AllowImportRules::No
    };
    parse(css, base, allow)
}

/// Exactly one rule, as `insertRule()` requires.
pub fn parse_rule(css: &str, base: Option<&Url>) -> Result<Rule, String> {
    let mut rules = parse(css, base, AllowImportRules::Yes);
    match rules.len() {
        1 => Ok(rules.remove(0)),
        0 => Err("the rule could not be parsed".to_string()),
        _ => Err("more than one rule was given".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_top_level_rules() {
        let rules = parse_rules(
            "@import url(x.css); p, div > a { color: red; margin: 0 } @media (min-width: 1px) { b { x: 1; display: block } } @font-face { font-family: F; src: url(f.woff) } nonsense {",
            None,
            true,
        );
        // `@import` needs a loader this engine does not have yet, so it is
        // dropped; the unclosed block at the end is a rule, as CSS says.
        let kinds: Vec<u16> = rules.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, [1, 4, 5, 1]);
        assert_eq!(rules[0].css_text, "p, div > a { color: red; margin: 0px; }");
        assert_eq!(rules[0].selector_text.as_deref(), Some("p, div > a"));
        assert_eq!(
            rules[1].css_text,
            "@media (min-width: 1px) {\n  b { display: block; }\n}"
        );
        assert_eq!(rules[3].css_text, "nonsense { }");
        assert_eq!(
            parse_rules("@import url(x.css); a { b: c }", None, false).len(),
            1
        );
    }

    #[test]
    fn one_rule_at_a_time() {
        assert_eq!(
            parse_rule("a { color: blue }", None).unwrap().kind,
            STYLE_RULE
        );
        assert!(parse_rule("a { color: blue } b { }", None).is_err());
        assert!(parse_rule("", None).is_err());
        assert!(
            parse_rule("a {", None).is_ok(),
            "an unclosed block still parses"
        );
        assert!(parse_rule("@nonsense x;", None).is_err());
    }
}
