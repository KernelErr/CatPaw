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

/// <https://dom.spec.whatwg.org/#find-flattened-slottables>: the nodes a
/// slot shows, with nested slots replaced by what they show, and the
/// slot's own children when nothing is assigned.
fn flattened(dom: &catpaw_dom::Dom, slot: NodeId) -> Vec<NodeId> {
    let mut assigned = dom.assigned_nodes(slot);
    if assigned.is_empty() {
        assigned = dom.children(slot).collect();
    }
    let mut out = Vec::new();
    for node in assigned {
        let is_slot = dom
            .element(node)
            .is_some_and(|el| el.is_html() && &*el.name.local == "slot");
        if is_slot {
            out.extend(flattened(dom, node));
        } else {
            out.push(node);
        }
    }
    out
}

impl web::HTMLSlotElementImpl for Web {
    fn assigned_nodes(
        cx: &mut Cx<'_>,
        this: NodeId,
        options: web::AssignedNodesOptions,
    ) -> Fallible<Vec<NodeId>> {
        node::check(cx, this)?;
        let dom = cx.dom();
        Ok(if options.flatten {
            flattened(&dom, this)
        } else {
            dom.assigned_nodes(this)
        })
    }

    fn assigned_elements(
        cx: &mut Cx<'_>,
        this: NodeId,
        options: web::AssignedNodesOptions,
    ) -> Fallible<Vec<NodeId>> {
        let nodes = Self::assigned_nodes(cx, this, options)?;
        let dom = cx.dom();
        Ok(nodes.into_iter().filter(|&n| dom.is_element(n)).collect())
    }
}

impl web::SlottableImpl for Web {
    fn assigned_slot(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        // A closed shadow tree keeps its slots to itself.
        let dom = cx.dom();
        let Some(slot) = dom.assigned_slot(this) else {
            return Ok(None);
        };
        let open = matches!(
            dom.kind(dom.root_of(slot)),
            NodeKind::DocumentFragment(FragmentKind::ShadowRoot { open: true, .. })
        );
        Ok(open.then_some(slot))
    }
}
