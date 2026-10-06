//! Media queries outside stylesheets (`window.matchMedia`).

use std::borrow::Cow;

use cssparser::Parser;
use selectors::matching::QuirksMode;
use style::custom_properties::AttrTaint;
use style::media_queries::MediaList;
use style::parser::ParserContext;
use style::servo_arc::Arc;
use style::stylesheets::{CssRuleType, CustomMediaEvaluator, Namespaces, Origin, UrlExtraData};
use style_traits::{ParsingMode, ToCss};
use url::Url;

use crate::engine::{StyleOptions, make_device, set_prefs};

/// A parsed media query list.
pub struct MediaQueryList(MediaList);

impl MediaQueryList {
    /// Parses a media query list. Like CSS, this never fails: a query that
    /// is not understood is kept, and never matches.
    pub fn parse(text: &str) -> Self {
        let url_data = UrlExtraData(Arc::new(Url::parse("about:blank").expect("valid URL")));
        let namespaces = Namespaces::default();
        let mut context = ParserContext::new(
            Origin::Author,
            &url_data,
            Some(CssRuleType::Media),
            ParsingMode::DEFAULT,
            QuirksMode::NoQuirks,
            Cow::Borrowed(&namespaces),
            None,
            None,
            AttrTaint::default(),
        );
        let mut parser = Parser::new(text);
        Self(MediaList::parse(&mut context, &mut parser))
    }

    /// The list in its serialized form.
    pub fn text(&self) -> String {
        self.0.to_css_string()
    }

    /// Whether the list matches a device with the given characteristics.
    pub fn matches(&self, options: &StyleOptions) -> bool {
        set_prefs();
        let device = make_device(options);
        self.0.evaluate(
            &device,
            QuirksMode::NoQuirks,
            &mut CustomMediaEvaluator::none(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_against_the_viewport() {
        let options = StyleOptions::default(); // 1280x720, light
        let matches = |q: &str| MediaQueryList::parse(q).matches(&options);
        assert!(matches(""));
        assert!(matches("screen"));
        assert!(!matches("print"));
        assert!(matches("(min-width: 800px)"));
        assert!(!matches("(max-width: 800px)"));
        assert!(matches("(min-width: 800px) and (orientation: landscape)"));
        assert!(matches("print, (width >= 1000px)"));
        assert!(!matches("(prefers-color-scheme: dark)"));
        assert!(matches("(prefers-color-scheme: light)"));
        assert!(!matches("(this-is-not: a-feature)"));

        let dark = StyleOptions {
            dark_mode: true,
            ..StyleOptions::default()
        };
        assert!(MediaQueryList::parse("(prefers-color-scheme: dark)").matches(&dark));
    }

    #[test]
    fn serializes_the_parsed_list() {
        assert_eq!(
            MediaQueryList::parse("SCREEN  and (min-width:10px)").text(),
            "screen and (min-width: 10px)"
        );
        let unknown = MediaQueryList::parse("(nonsense");
        assert!(!unknown.matches(&StyleOptions::default()));
        assert!(!unknown.text().is_empty());
    }
}
