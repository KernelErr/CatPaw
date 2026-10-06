//! Shadow trees: `attachShadow()` and `ShadowRoot`.
//!
//! A shadow tree hangs off its host without being among its children; it
//! is connected when the host is, and events dispatched inside it reach
//! the host and beyond, retargeted (see `events`). Slots do not assign
//! anything yet, and styles are not scoped to shadow trees.

use catpaw_dom::{FragmentKind, NodeId, NodeKind};
use catpaw_js::{Exception, Fallible};

use crate::generated::{self as web, ShadowRootMode, SlotAssignmentMode};
use crate::page::Cx;
use crate::{Web, custom_elements, node};

/// The elements that may have a shadow tree, besides custom elements.
const HOSTS: &[&str] = &[
    "article",
    "aside",
    "blockquote",
    "body",
    "div",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "main",
    "nav",
    "p",
    "section",
    "span",
];

/// <https://dom.spec.whatwg.org/#dom-element-attachshadow>
pub(crate) fn attach(cx: &mut Cx<'_>, host: NodeId, init: web::ShadowRootInit) -> Fallible<NodeId> {
    node::check(cx, host)?;
    {
        let dom = cx.dom();
        let Some(data) = dom.element(host) else {
            return Err(Exception::not_supported(
                "Only elements can host a shadow tree",
            ));
        };
        let local = &*data.name.local;
        let allowed = data.is_html()
            && (HOSTS.contains(&local)
                || custom_elements::is_valid_custom_element_name(local)
                || data
                    .is_value
                    .as_deref()
                    .is_some_and(custom_elements::is_valid_custom_element_name));
        if !allowed {
            return Err(Exception::not_supported(format!(
                "A <{local}> element cannot host a shadow tree"
            )));
        }
        if data.shadow_root.is_some() {
            return Err(Exception::not_supported(
                "The element already hosts a shadow tree",
            ));
        }
    }
    let kind = FragmentKind::ShadowRoot {
        host,
        open: init.mode == ShadowRootMode::Open,
        delegates_focus: init.delegates_focus,
        clonable: init.clonable,
        serializable: init.serializable,
    };
    let mut dom = cx.dom_mut();
    let shadow = dom.create_fragment(kind);
    let document = dom.owner_document(host);
    dom.adopt_subtree(shadow, document);
    if let Some(data) = dom.element_mut(host) {
        data.shadow_root = Some(shadow);
    }
    Ok(shadow)
}

/// What a shadow root was attached with.
struct Facts {
    host: NodeId,
    open: bool,
    delegates_focus: bool,
    clonable: bool,
    serializable: bool,
}

fn facts(cx: &Cx<'_>, this: NodeId) -> Fallible<Facts> {
    node::check(cx, this)?;
    match cx.dom().kind(this) {
        NodeKind::DocumentFragment(FragmentKind::ShadowRoot {
            host,
            open,
            delegates_focus,
            clonable,
            serializable,
        }) => Ok(Facts {
            host: *host,
            open: *open,
            delegates_focus: *delegates_focus,
            clonable: *clonable,
            serializable: *serializable,
        }),
        _ => Err(Exception::type_error("The node is not a shadow root")),
    }
}

impl web::ShadowRootImpl for Web {
    fn mode(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ShadowRootMode> {
        Ok(if facts(cx, this)?.open {
            ShadowRootMode::Open
        } else {
            ShadowRootMode::Closed
        })
    }

    fn delegates_focus(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        Ok(facts(cx, this)?.delegates_focus)
    }

    fn serializable(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        Ok(facts(cx, this)?.serializable)
    }

    fn slot_assignment(cx: &mut Cx<'_>, this: NodeId) -> Fallible<SlotAssignmentMode> {
        facts(cx, this)?;
        Ok(SlotAssignmentMode::Named)
    }

    fn clonable(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        Ok(facts(cx, this)?.clonable)
    }

    fn host(cx: &mut Cx<'_>, this: NodeId) -> Fallible<NodeId> {
        Ok(facts(cx, this)?.host)
    }

    fn inner_html(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        facts(cx, this)?;
        Ok(catpaw_dom::to_html(&cx.dom(), this, true))
    }

    fn set_inner_html(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        let host = facts(cx, this)?.host;
        let fragment = crate::element::parse_fragment_for(cx, &value, host);
        node::replace_all(cx, Some(fragment), this);
        Ok(())
    }
}
