//! Computed styles as script sees them: `getComputedStyle()`.

use std::sync::OnceLock;

use style::properties::{
    ComputedValues, Importance, NonCustomPropertyId, PropertyDeclarationBlock,
    PropertyDeclarationId, PropertyId,
};
use style::servo_arc::Arc;
use style::values::resolved;

/// A pseudo-element whose style can be asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pseudo {
    Before,
    After,
}

impl Pseudo {
    /// Parses the second argument of `getComputedStyle()`: `::before` and
    /// `::after`, also in their one-colon spelling.
    pub fn parse(text: &str) -> Option<Self> {
        let name = text.strip_prefix("::").or_else(|| text.strip_prefix(':'))?;
        if name.eq_ignore_ascii_case("before") {
            Some(Pseudo::Before)
        } else if name.eq_ignore_ascii_case("after") {
            Some(Pseudo::After)
        } else {
            None
        }
    }
}

/// The computed style of an element or pseudo-element.
#[derive(Clone)]
pub struct ComputedStyle(pub(crate) Arc<ComputedValues>);

impl ComputedStyle {
    /// The serialized value of `property`: a longhand, a shorthand or a
    /// custom property. Empty when there is no such property, or when a
    /// shorthand cannot stand for the values of its longhands.
    ///
    /// Without layout these are computed values throughout: lengths that
    /// depend on a box (`width: auto`, percentages) are not resolved.
    pub fn get(&self, property: &str) -> String {
        let Ok(id) = PropertyId::parse_enabled_for_all_content(property) else {
            return String::new();
        };
        let values = &*self.0;
        let id = match id {
            PropertyId::Custom(name) => {
                return values.computed_value_to_string(PropertyDeclarationId::Custom(&name));
            }
            PropertyId::NonCustom(id) => id,
        };
        match id.longhand_or_shorthand() {
            Ok(longhand) => {
                values.computed_value_to_string(PropertyDeclarationId::Longhand(longhand))
            }
            Err(shorthand) => {
                let mut block = PropertyDeclarationBlock::new();
                for longhand in shorthand.longhands() {
                    let mut context = resolved::Context {
                        style: values,
                        for_property: PropertyId::NonCustom(longhand.into()),
                        current_longhand: Some(longhand),
                    };
                    let declaration =
                        values.computed_or_resolved_declaration(longhand, Some(&mut context));
                    block.push(declaration, Importance::Normal);
                }
                let mut css = String::new();
                match block.shorthand_to_css(shorthand, &mut css) {
                    Ok(()) => css,
                    Err(_) => String::new(),
                }
            }
        }
    }

    /// The names of the custom properties that have a value, with their
    /// leading dashes.
    pub fn custom_properties(&self) -> Vec<String> {
        let properties = self.0.custom_properties();
        let mut names: Vec<String> = properties
            .inherited
            .iter()
            .chain(properties.non_inherited.iter())
            .map(|(name, _)| format!("--{name}"))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// The longhand properties a computed style lists, in alphabetical order.
pub fn longhand_names() -> &'static [&'static str] {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        crate::engine::set_prefs();
        let mut names: Vec<&'static str> = NonCustomPropertyId::iter()
            .filter(|id| PropertyId::NonCustom(*id).enabled_for_all_content())
            .filter_map(|id| id.as_longhand())
            .map(|id| id.name())
            .collect();
        names.sort_unstable();
        names
    })
}
