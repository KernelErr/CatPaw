//! The CSSOM view of inline styles: `element.style`.
//!
//! The `style` content attribute is the single source of truth. Each
//! operation parses it, edits the declaration block and writes the block
//! back, so the attribute and the object can never disagree.

use catpaw_dom::NodeId;
use catpaw_js::{Fallible, ObjectId};
use catpaw_style::InlineStyle;
use catpaw_style::inline::{idl_to_css_property, is_supported_property};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, element, platform_object};

/// The inline style of an element.
pub struct InlineStyleObject {
    element: NodeId,
}
platform_object!(InlineStyleObject, CSSStyleProperties);

fn owner(cx: &Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
    cx.page.with::<InlineStyleObject, _>(this, |s| s.element)
}

fn read<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&InlineStyle) -> R) -> Fallible<R> {
    let element = owner(cx, this)?;
    let attribute = element::get_attr(cx, element, "style").unwrap_or_default();
    Ok(f(&InlineStyle::parse(&attribute)))
}

/// Edits the block with `f`, which reports whether it changed anything,
/// and writes it back to the `style` attribute if so.
fn edit<R>(
    cx: &mut Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut InlineStyle) -> (bool, R),
) -> Fallible<R> {
    let element = owner(cx, this)?;
    let attribute = element::get_attr(cx, element, "style").unwrap_or_default();
    let mut style = InlineStyle::parse(&attribute);
    let (changed, result) = f(&mut style);
    if changed {
        element::set_attr(cx, element, "style", style.css_text())?;
    }
    Ok(result)
}

/// The CSS property a property attribute name refers to.
fn attribute_property(name: &str) -> Option<String> {
    idl_to_css_property(name)
}

impl web::ElementCSSInlineStyleImpl for Web {
    fn style(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(InlineStyleObject { element: this }))
    }
}

impl web::CSSStyleDeclarationImpl for Web {
    fn css_text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, InlineStyle::css_text)
    }

    fn set_css_text(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let element = owner(cx, this)?;
        let normalized = InlineStyle::parse(&value).css_text();
        element::set_attr(cx, element, "style", normalized)
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        read(cx, this, |s| s.len() as u32)
    }

    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<String> {
        read(cx, this, |s| s.item(index as usize).unwrap_or_default())
    }

    fn get_property_value(cx: &mut Cx<'_>, this: ObjectId, property: String) -> Fallible<String> {
        read(cx, this, |s| s.get(&property))
    }

    fn get_property_priority(
        cx: &mut Cx<'_>,
        this: ObjectId,
        property: String,
    ) -> Fallible<String> {
        read(cx, this, |s| s.priority(&property).to_string())
    }

    fn set_property(
        cx: &mut Cx<'_>,
        this: ObjectId,
        property: String,
        value: String,
        priority: String,
    ) -> Fallible<()> {
        if !is_supported_property(&property) {
            return Ok(());
        }
        let important = priority.eq_ignore_ascii_case("important");
        if !priority.is_empty() && !important {
            return Ok(());
        }
        edit(cx, this, |s| (s.set(&property, &value, important), ()))
    }

    fn remove_property(cx: &mut Cx<'_>, this: ObjectId, property: String) -> Fallible<String> {
        edit(cx, this, |s| {
            let old = s.remove(&property);
            (!old.is_empty(), old)
        })
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<String>> {
        read(cx, this, |s| s.item(index as usize))
    }
}

impl web::CSSStylePropertiesImpl for Web {
    fn css_float(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        read(cx, this, |s| s.get("float"))
    }

    fn set_css_float(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |s| (s.set("float", &value, false), ()))
    }

    fn named_get(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<Option<String>> {
        let Some(property) = attribute_property(name) else {
            return Ok(None);
        };
        read(cx, this, |s| Some(s.get(&property)))
    }

    fn named_properties(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Vec<String>> {
        // Property attributes live on the prototype in other engines; they
        // are not own properties of a style object.
        Ok(Vec::new())
    }

    fn named_set(cx: &mut Cx<'_>, this: ObjectId, name: &str, value: String) -> Fallible<()> {
        let Some(property) = attribute_property(name) else {
            return Ok(());
        };
        edit(cx, this, |s| (s.set(&property, &value, false), ()))
    }
}
