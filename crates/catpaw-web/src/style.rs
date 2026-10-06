//! `CSSStyleDeclaration` objects: `element.style` and the result of
//! `getComputedStyle()`.
//!
//! For inline styles the `style` content attribute is the single source of
//! truth. Each operation parses it, edits the declaration block and writes
//! the block back, so the attribute and the object can never disagree.
//!
//! A computed style is a live, read-only view: every read asks the style
//! engine, which resolves the element's style again if the document has
//! changed since.

use catpaw_dom::NodeId;
use catpaw_js::{Exception, Fallible, ObjectId};
use catpaw_style::computed::longhand_names;
use catpaw_style::inline::{idl_to_css_property, is_supported_property};
use catpaw_style::{InlineStyle, Pseudo};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, element, platform_object, stylesheets};

/// The inline style of an element.
pub struct InlineStyleObject {
    element: NodeId,
}
platform_object!(InlineStyleObject, CSSStyleProperties);

/// Whose computed style a declaration shows.
#[derive(Clone, Copy)]
enum Subject {
    Element,
    Pseudo(Pseudo),
    /// A pseudo-element there are no styles for: nothing has a value.
    Nothing,
}

/// The computed style of an element or pseudo-element.
pub struct ComputedStyleObject {
    element: NodeId,
    subject: Subject,
}
platform_object!(ComputedStyleObject, CSSStyleProperties);

/// `getComputedStyle(element, pseudo)`.
pub(crate) fn computed_style(cx: &Cx<'_>, element: NodeId, pseudo: Option<&str>) -> ObjectId {
    // Only an argument that starts with a colon names a pseudo-element.
    let subject = match pseudo.filter(|p| p.starts_with(':')) {
        None => Subject::Element,
        Some(selector) => Pseudo::parse(selector).map_or(Subject::Nothing, Subject::Pseudo),
    };
    cx.page.alloc(ComputedStyleObject { element, subject })
}

/// What a declaration object is a view of.
enum Declaration {
    Inline(NodeId),
    Computed(NodeId, Subject),
}

fn declaration(cx: &Cx<'_>, this: ObjectId) -> Fallible<Declaration> {
    let inline = cx
        .page
        .try_with::<InlineStyleObject, _>(this, |style| style.element);
    if let Some(element) = inline {
        return Ok(Declaration::Inline(element));
    }
    cx.page.with::<ComputedStyleObject, _>(this, |style| {
        Declaration::Computed(style.element, style.subject)
    })
}

fn read_only() -> Exception {
    Exception::no_modification_allowed("A computed style cannot be changed")
}

fn computed_value(cx: &Cx<'_>, element: NodeId, subject: Subject, property: &str) -> String {
    let pseudo = match subject {
        Subject::Element => None,
        Subject::Pseudo(pseudo) => Some(pseudo),
        Subject::Nothing => return String::new(),
    };
    stylesheets::computed_value(cx.page, element, pseudo, property)
}

/// The properties a computed style lists: every longhand, as long as its
/// element is in the document.
fn computed_names(cx: &Cx<'_>, element: NodeId, subject: Subject) -> &'static [&'static str] {
    let dom = cx.dom();
    let styled = dom.contains(element) && dom.is_connected(element);
    if styled && !matches!(subject, Subject::Nothing) {
        longhand_names()
    } else {
        &[]
    }
}

/// Reads from the declaration: `inline` sees the parsed `style` attribute,
/// `computed` answers for a computed style.
fn read<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    inline: impl FnOnce(&InlineStyle) -> R,
    computed: impl FnOnce(NodeId, Subject) -> R,
) -> Fallible<R> {
    Ok(match declaration(cx, this)? {
        Declaration::Inline(element) => {
            let attribute = element::get_attr(cx, element, "style").unwrap_or_default();
            inline(&InlineStyle::parse(&attribute))
        }
        Declaration::Computed(element, subject) => computed(element, subject),
    })
}

/// Edits an inline style with `f`, which reports whether it changed
/// anything, and writes it back to the `style` attribute if so. A computed
/// style cannot be edited.
fn edit<R>(
    cx: &mut Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut InlineStyle) -> (bool, R),
) -> Fallible<R> {
    let Declaration::Inline(element) = declaration(cx, this)? else {
        return Err(read_only());
    };
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
        read(cx, this, InlineStyle::css_text, |_, _| String::new())
    }

    fn set_css_text(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let Declaration::Inline(element) = declaration(cx, this)? else {
            return Err(read_only());
        };
        let normalized = InlineStyle::parse(&value).css_text();
        element::set_attr(cx, element, "style", normalized)
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        read(
            cx,
            this,
            |style| style.len() as u32,
            |element, subject| computed_names(cx, element, subject).len() as u32,
        )
    }

    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<String> {
        Ok(Self::indexed_get(cx, this, index)?.unwrap_or_default())
    }

    fn get_property_value(cx: &mut Cx<'_>, this: ObjectId, property: String) -> Fallible<String> {
        read(
            cx,
            this,
            |style| style.get(&property),
            |element, subject| computed_value(cx, element, subject, &property),
        )
    }

    fn get_property_priority(
        cx: &mut Cx<'_>,
        this: ObjectId,
        property: String,
    ) -> Fallible<String> {
        read(
            cx,
            this,
            |style| style.priority(&property).to_string(),
            |_, _| String::new(),
        )
    }

    fn set_property(
        cx: &mut Cx<'_>,
        this: ObjectId,
        property: String,
        value: String,
        priority: String,
    ) -> Fallible<()> {
        if matches!(declaration(cx, this)?, Declaration::Computed(..)) {
            return Err(read_only());
        }
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
        read(
            cx,
            this,
            |style| style.item(index as usize),
            |element, subject| {
                computed_names(cx, element, subject)
                    .get(index as usize)
                    .map(|name| name.to_string())
            },
        )
    }
}

impl web::CSSStylePropertiesImpl for Web {
    fn css_float(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        <Web as web::CSSStyleDeclarationImpl>::get_property_value(cx, this, "float".to_string())
    }

    fn set_css_float(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        edit(cx, this, |s| (s.set("float", &value, false), ()))
    }

    fn named_get(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<Option<String>> {
        let Some(property) = attribute_property(name) else {
            return Ok(None);
        };
        <Web as web::CSSStyleDeclarationImpl>::get_property_value(cx, this, property).map(Some)
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

impl web::CSSImpl for Web {
    fn supports(_cx: &mut Cx<'_>, property: String, value: String) -> Fallible<bool> {
        Ok(catpaw_style::supports::supports_declaration(
            &property, &value,
        ))
    }

    fn supports_overload2(_cx: &mut Cx<'_>, condition_text: String) -> Fallible<bool> {
        Ok(catpaw_style::supports::supports(&condition_text))
    }

    fn escape(_cx: &mut Cx<'_>, ident: String) -> Fallible<String> {
        Ok(catpaw_style::supports::escape(&ident))
    }
}
