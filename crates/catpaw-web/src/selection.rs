//! The Selection API, for a page with nothing selected.
//!
//! There is no `Range` yet, so the selection has no ranges: it reports
//! itself empty (`type` "None", `rangeCount` 0) and takes the calls that
//! would set it without complaint, after checking their arguments as the
//! specification does. Pages that read the selection before moving focus
//! or restoring scroll positions, as routers do, carry on.

use catpaw_dom::{NodeId, NodeKind};
use catpaw_js::{Exception, Fallible, ObjectId};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, node, platform_object};

pub struct SelectionObject;
platform_object!(SelectionObject, Selection);

/// The document's one selection object.
pub(crate) fn selection(cx: &mut Cx<'_>) -> ObjectId {
    crate::window::singleton(cx, |s| &mut s.selection, |page| page.alloc(SelectionObject))
}

/// <https://dom.spec.whatwg.org/#concept-node-length>
fn node_length(cx: &Cx<'_>, node: NodeId) -> u32 {
    let dom = cx.dom();
    match dom.kind(node) {
        NodeKind::Doctype(_) => 0,
        NodeKind::Text(t) | NodeKind::Comment(t) => node::utf16_len(t),
        NodeKind::ProcessingInstruction { data, .. } => node::utf16_len(data),
        _ => dom.children(node).count() as u32,
    }
}

fn check_offset(cx: &Cx<'_>, node: NodeId, offset: u32) -> Fallible<()> {
    node::check(cx, node)?;
    if offset > node_length(cx, node) {
        return Err(Exception::index_size(
            "The offset is larger than the node's length.",
        ));
    }
    Ok(())
}

impl web::SelectionImpl for Web {
    fn anchor_node(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Option<NodeId>> {
        Ok(None)
    }

    fn anchor_offset(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u32> {
        Ok(0)
    }

    fn focus_node(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Option<NodeId>> {
        Ok(None)
    }

    fn focus_offset(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u32> {
        Ok(0)
    }

    fn is_collapsed(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<bool> {
        Ok(true)
    }

    fn range_count(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u32> {
        Ok(0)
    }

    fn type_(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("None".to_string())
    }

    fn direction(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("none".to_string())
    }

    fn remove_all_ranges(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        Ok(())
    }

    fn empty(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        Ok(())
    }

    fn collapse(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        node: Option<NodeId>,
        offset: u32,
    ) -> Fallible<()> {
        if let Some(node) = node {
            check_offset(cx, node, offset)?;
        }
        Ok(())
    }

    fn set_position(
        cx: &mut Cx<'_>,
        this: ObjectId,
        node: Option<NodeId>,
        offset: u32,
    ) -> Fallible<()> {
        Self::collapse(cx, this, node, offset)
    }

    fn collapse_to_start(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        Err(Exception::invalid_state(
            "There is no selection to collapse.",
        ))
    }

    fn collapse_to_end(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        Err(Exception::invalid_state(
            "There is no selection to collapse.",
        ))
    }

    fn extend(cx: &mut Cx<'_>, _this: ObjectId, node: NodeId, offset: u32) -> Fallible<()> {
        check_offset(cx, node, offset)?;
        Err(Exception::invalid_state("There is no selection to extend."))
    }

    fn set_base_and_extent(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        anchor_node: NodeId,
        anchor_offset: u32,
        focus_node: NodeId,
        focus_offset: u32,
    ) -> Fallible<()> {
        check_offset(cx, anchor_node, anchor_offset)?;
        check_offset(cx, focus_node, focus_offset)?;
        Ok(())
    }

    fn select_all_children(cx: &mut Cx<'_>, _this: ObjectId, node: NodeId) -> Fallible<()> {
        node::check(cx, node)?;
        if matches!(cx.dom().kind(node), NodeKind::Doctype(_)) {
            return Err(Exception::invalid_node_type(
                "A doctype cannot be selected.",
            ));
        }
        Ok(())
    }

    fn modify(
        _cx: &mut Cx<'_>,
        _this: ObjectId,
        _alter: Option<String>,
        _direction: Option<String>,
        _granularity: Option<String>,
    ) -> Fallible<()> {
        Ok(())
    }

    fn delete_from_document(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        Ok(())
    }

    fn contains_node(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        node: NodeId,
        _allow_partial_containment: bool,
    ) -> Fallible<bool> {
        node::check(cx, node)?;
        Ok(false)
    }

    fn stringify(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok(String::new())
    }
}
