//! SVG elements: the interfaces script tells them apart by, and the string
//! attributes that are objects there (`className`, `href`).
//!
//! Not there yet: geometry (`getBBox()` and the like needs layout) and the
//! animated values other than strings.

use catpaw_dom::{Dom, NodeId};
use catpaw_js::{Fallible, ObjectId};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, element, platform_object};

pub(crate) const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

#[derive(Clone, Copy)]
enum Reflected {
    /// The `class` attribute.
    Class,
    /// The `href` attribute, or `xlink:href` where only that is present.
    Href,
}

/// An `SVGAnimatedString`: a view of one attribute of an element. Nothing
/// animates, so the animated value is the base value.
pub struct AnimatedStringObject {
    element: NodeId,
    reflected: Reflected,
}
platform_object!(AnimatedStringObject, SVGAnimatedString);

fn xlink_href(dom: &Dom, element: NodeId) -> Option<String> {
    dom.element(element)?
        .attrs
        .iter()
        .find(|a| &*a.name.ns == XLINK_NS && &*a.name.local == "href")
        .map(|a| a.value.to_string())
}

/// The nearest ancestor `svg` element.
fn owner_svg(dom: &Dom, element: NodeId) -> Option<NodeId> {
    dom.ancestors(element).find(|&ancestor| {
        dom.element(ancestor)
            .is_some_and(|el| &*el.name.ns == SVG_NS && &*el.name.local == "svg")
    })
}

impl web::SVGElementImpl for Web {
    fn class_name(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(AnimatedStringObject {
            element: this,
            reflected: Reflected::Class,
        }))
    }

    fn owner_svg_element(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(owner_svg(&cx.dom(), this))
    }

    fn viewport_element(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(owner_svg(&cx.dom(), this))
    }
}

impl web::SVGURIReferenceImpl for Web {
    fn href(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(AnimatedStringObject {
            element: this,
            reflected: Reflected::Href,
        }))
    }
}

fn animated(cx: &Cx<'_>, this: ObjectId) -> Fallible<(NodeId, Reflected)> {
    cx.page
        .with::<AnimatedStringObject, _>(this, |s| (s.element, s.reflected))
}

impl web::SVGAnimatedStringImpl for Web {
    fn base_val(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        let (element, reflected) = animated(cx, this)?;
        let value = match reflected {
            Reflected::Class => element::get_attr(cx, element, "class"),
            Reflected::Href => {
                element::get_attr(cx, element, "href").or_else(|| xlink_href(&cx.dom(), element))
            }
        };
        Ok(value.unwrap_or_default())
    }

    fn set_base_val(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let (element, reflected) = animated(cx, this)?;
        match reflected {
            Reflected::Class => element::set_attr(cx, element, "class", value),
            Reflected::Href => {
                let only_xlink = element::get_attr(cx, element, "href").is_none()
                    && xlink_href(&cx.dom(), element).is_some();
                if only_xlink {
                    <Web as web::ElementImpl>::set_attribute_ns(
                        cx,
                        element,
                        Some(XLINK_NS.to_string()),
                        "xlink:href".to_string(),
                        value,
                    )
                } else {
                    element::set_attr(cx, element, "href", value)
                }
            }
        }
    }

    fn anim_val(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Self::base_val(cx, this)
    }
}
