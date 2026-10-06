//! Inline style declarations: the block behind an element's `style`
//! attribute, as the CSSOM (`element.style`) reads and edits it.
//!
//! Parsing and serialization are Stylo's, so values are validated and
//! normalized the way they are everywhere else in the style system.

use selectors::matching::QuirksMode;
use style::properties::{
    Importance, PropertyDeclarationBlock, PropertyId, SourcePropertyDeclaration,
    parse_one_declaration_into, parse_style_attribute,
};
use style::servo_arc::Arc;
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style_traits::{CssStringWriter, ParsingMode};
use url::Url;

thread_local! {
    static URL_DATA: UrlExtraData =
        UrlExtraData(Arc::new(Url::parse("about:blank").expect("valid URL")));
}

/// Parses a property name as CSS writes it. Custom properties keep their
/// case; everything else is ASCII case-insensitive.
fn property_id(name: &str) -> Option<PropertyId> {
    if name.starts_with("--") {
        PropertyId::parse_enabled_for_all_content(name).ok()
    } else {
        PropertyId::parse_enabled_for_all_content(&name.to_ascii_lowercase()).ok()
    }
}

/// Whether the style system knows the CSS property `name`.
pub fn is_supported_property(name: &str) -> bool {
    property_id(name).is_some()
}

/// Maps a CSSOM IDL attribute name to the CSS property it stands for:
/// `backgroundColor` and `background-color` to `background-color`,
/// `cssFloat` to `float`, `webkitTransform` to `-webkit-transform`.
/// `None` if no supported property has that attribute.
pub fn idl_to_css_property(name: &str) -> Option<String> {
    if name == "cssFloat" {
        return Some("float".to_string());
    }
    if name.is_empty() || name.starts_with('-') {
        return None;
    }
    let mut css = String::with_capacity(name.len() + 4);
    // The webkit-cased attribute spells its vendor prefix in lowercase.
    let webkit_cased = name
        .strip_prefix("webkit")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()));
    if webkit_cased {
        css.push('-');
    }
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            css.push('-');
            css.push(c.to_ascii_lowercase());
        } else {
            css.push(c);
        }
    }
    PropertyId::parse_enabled_for_all_content(&css)
        .is_ok()
        .then_some(css)
}

/// The declarations of one `style` attribute.
pub struct InlineStyle(PropertyDeclarationBlock);

impl InlineStyle {
    /// Parses the value of a `style` attribute. Invalid declarations are
    /// dropped, as the cascade would drop them.
    pub fn parse(attribute: &str) -> Self {
        URL_DATA.with(|url_data| {
            Self(parse_style_attribute(
                attribute,
                url_data,
                None,
                QuirksMode::NoQuirks,
                CssRuleType::Style,
            ))
        })
    }

    /// The block serialized as a declaration list (`cssText`).
    pub fn css_text(&self) -> String {
        let mut out = CssStringWriter::new();
        let _ = self.0.to_css(&mut out);
        out.to_string()
    }

    /// The number of declarations (longhands).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The property name of the declaration at `index`.
    pub fn item(&self, index: usize) -> Option<String> {
        self.0
            .declarations()
            .get(index)
            .map(|d| d.id().name().into_owned())
    }

    /// The value of `property` (a longhand, shorthand or custom property),
    /// or the empty string if it is not set or not a known property.
    pub fn get(&self, property: &str) -> String {
        let Some(id) = property_id(property) else {
            return String::new();
        };
        let mut out = CssStringWriter::new();
        let _ = self.0.property_value_to_css(&id, &mut out);
        out.to_string()
    }

    /// `"important"` if `property` is set with `!important`.
    pub fn priority(&self, property: &str) -> &'static str {
        match property_id(property) {
            Some(id) if self.0.property_priority(&id).important() => "important",
            _ => "",
        }
    }

    /// Sets `property` to `value`; an empty value removes it. Returns
    /// whether the block changed (an invalid value changes nothing).
    pub fn set(&mut self, property: &str, value: &str, important: bool) -> bool {
        let Some(id) = property_id(property) else {
            return false;
        };
        if value.trim().is_empty() {
            return self.remove_id(&id);
        }
        let mut declarations = SourcePropertyDeclaration::default();
        let parsed = URL_DATA.with(|url_data| {
            parse_one_declaration_into(
                &mut declarations,
                id,
                value,
                Origin::Author,
                url_data,
                None,
                ParsingMode::DEFAULT,
                QuirksMode::NoQuirks,
                CssRuleType::Style,
            )
        });
        if parsed.is_err() {
            return false;
        }
        let importance = if important {
            Importance::Important
        } else {
            Importance::Normal
        };
        self.0.extend(declarations.drain(), importance)
    }

    fn remove_id(&mut self, id: &PropertyId) -> bool {
        match self.0.first_declaration_to_remove(id) {
            Some(first) => {
                self.0.remove_property(id, first);
                true
            }
            None => false,
        }
    }

    /// Removes `property` and returns the value it had.
    pub fn remove(&mut self, property: &str) -> String {
        let Some(id) = property_id(property) else {
            return String::new();
        };
        let old = self.get(property);
        self.remove_id(&id);
        old
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_idl_names_to_properties() {
        assert_eq!(idl_to_css_property("color").as_deref(), Some("color"));
        assert_eq!(
            idl_to_css_property("backgroundColor").as_deref(),
            Some("background-color")
        );
        assert_eq!(
            idl_to_css_property("background-color").as_deref(),
            Some("background-color")
        );
        assert_eq!(idl_to_css_property("cssFloat").as_deref(), Some("float"));
        assert_eq!(idl_to_css_property("notAProperty"), None);
        assert_eq!(idl_to_css_property("Background-Color"), None);
        assert_eq!(idl_to_css_property("--custom"), None);
        assert!(is_supported_property("DISPLAY"));
        assert!(is_supported_property("--anything"));
        assert!(!is_supported_property("nonsense"));
    }

    #[test]
    fn edits_a_declaration_block() {
        let mut style = InlineStyle::parse("COLOR: RED; bogus: 1; margin: 1px 2px");
        assert_eq!(style.get("color"), "red");
        assert_eq!(style.get("margin-left"), "2px");
        assert_eq!(style.get("margin"), "1px 2px");
        assert_eq!(style.get("bogus"), "");
        assert_eq!(style.len(), 5);
        assert_eq!(style.item(0).as_deref(), Some("color"));

        assert!(style.set("display", "none", true));
        assert_eq!(style.priority("display"), "important");
        assert!(
            !style.set("width", "not-a-length", false),
            "invalid values are ignored"
        );
        assert!(style.set("width", "10PX", false));
        assert_eq!(style.get("width"), "10px");
        assert!(style.set("--accent", " #0af ", false));
        assert_eq!(style.get("--accent"), "#0af");

        assert_eq!(style.remove("margin"), "1px 2px");
        assert!(style.set("color", "", false), "an empty value removes");
        assert_eq!(
            style.css_text(),
            "display: none !important; width: 10px; --accent: #0af;"
        );
        assert!(InlineStyle::parse("").is_empty());
    }
}
