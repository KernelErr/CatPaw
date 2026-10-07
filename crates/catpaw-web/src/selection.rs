//! The Selection API, over one live range.
//!
//! The document's selection holds at most one range, as browsers do, with
//! a direction: the anchor is the range's start unless the selection was
//! extended backwards. Nothing selects on its own here (there is no
//! pointer or keyboard), so the selection is what script makes it; the
//! `selectionchange` event is not fired yet.

use std::cmp::Ordering;

use catpaw_dom::{NodeId, NodeKind};
use catpaw_js::{Exception, Fallible, ObjectId};

use crate::generated as web;
use crate::page::Cx;
use crate::range::{self, Boundary};
use crate::{Web, node, platform_object};

pub struct SelectionObject {
    /// The one range, pinned while selected.
    range: Option<ObjectId>,
    backwards: bool,
}
platform_object!(SelectionObject, Selection);

/// The document's one selection object.
pub(crate) fn selection(cx: &mut Cx<'_>) -> ObjectId {
    crate::window::singleton(
        cx,
        |s| &mut s.selection,
        |page| {
            page.alloc(SelectionObject {
                range: None,
                backwards: false,
            })
        },
    )
}

fn with<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut SelectionObject) -> R) -> Fallible<R> {
    cx.page.with::<SelectionObject, _>(this, f)
}

/// The selected range, if it is still there.
fn current(cx: &Cx<'_>, this: ObjectId) -> Fallible<Option<ObjectId>> {
    Ok(with(cx, this, |s| s.range)?.filter(|&id| cx.page.object_exists(id)))
}

/// Replaces the selection's range, releasing the old one.
fn set_range(
    cx: &mut Cx<'_>,
    this: ObjectId,
    range: Option<ObjectId>,
    backwards: bool,
) -> Fallible<()> {
    let old = with(cx, this, |s| {
        let old = s.range;
        s.range = range;
        s.backwards = backwards;
        old
    })?;
    if let Some(id) = range {
        cx.pin(id);
    }
    if let Some(id) = old {
        cx.unpin(id);
    }
    Ok(())
}

/// Whether `node` belongs to the page's document, the only one a
/// selection can be in.
fn in_this_document(cx: &Cx<'_>, node: NodeId) -> bool {
    let dom = cx.dom();
    dom.owner_document(node) == cx.page.document()
}

fn check_offset(cx: &Cx<'_>, node: NodeId, offset: u32) -> Fallible<()> {
    node::check(cx, node)?;
    if offset > range::node_length(&cx.dom(), node) {
        return Err(Exception::index_size(
            "The offset is larger than the node's length.",
        ));
    }
    Ok(())
}

fn anchor_and_focus(cx: &Cx<'_>, this: ObjectId) -> Fallible<Option<(Boundary, Boundary)>> {
    let Some(range) = current(cx, this)? else {
        return Ok(None);
    };
    let (start, end) = range::bounds_of(cx, range)?;
    let backwards = with(cx, this, |s| s.backwards)?;
    Ok(Some(if backwards {
        (end, start)
    } else {
        (start, end)
    }))
}

/// Selects from `anchor` to `focus`, whichever comes first in the tree.
fn select(cx: &mut Cx<'_>, this: ObjectId, anchor: Boundary, focus: Boundary) -> Fallible<()> {
    // Points in different trees (one inside a shadow tree) cannot bound
    // one range: the selection collapses at the focus.
    let anchor = if cx.dom().root_of(anchor.node) == cx.dom().root_of(focus.node) {
        anchor
    } else {
        focus
    };
    let backwards = range::compare_points(&cx.dom(), focus, anchor) == Ordering::Less;
    let (start, end) = if backwards {
        (focus, anchor)
    } else {
        (anchor, focus)
    };
    let range = range::new_range(cx, start, end);
    set_range(cx, this, Some(range), backwards)
}

impl web::SelectionImpl for Web {
    fn anchor_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        Ok(anchor_and_focus(cx, this)?.map(|(a, _)| a.node))
    }

    fn anchor_offset(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        Ok(anchor_and_focus(cx, this)?.map_or(0, |(a, _)| a.offset))
    }

    fn focus_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        Ok(anchor_and_focus(cx, this)?.map(|(_, f)| f.node))
    }

    fn focus_offset(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        Ok(anchor_and_focus(cx, this)?.map_or(0, |(_, f)| f.offset))
    }

    fn is_collapsed(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        Ok(anchor_and_focus(cx, this)?.is_none_or(|(a, f)| a == f))
    }

    fn range_count(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        Ok(u32::from(current(cx, this)?.is_some()))
    }

    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Ok(match anchor_and_focus(cx, this)? {
            None => "None",
            Some((a, f)) if a == f => "Caret",
            Some(_) => "Range",
        }
        .to_string())
    }

    fn direction(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        let (has_range, backwards) = with(cx, this, |s| (s.range.is_some(), s.backwards))?;
        Ok(match (has_range, backwards) {
            (false, _) => "none",
            (true, true) => "backward",
            (true, false) => "forward",
        }
        .to_string())
    }

    fn get_range_at(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<ObjectId> {
        match current(cx, this)? {
            Some(range) if index == 0 => Ok(range),
            _ => Err(Exception::index_size(
                "The index is not 0, or there is no range.",
            )),
        }
    }

    /// Adds a range if there is none: a selection holds one.
    fn add_range(cx: &mut Cx<'_>, this: ObjectId, range: ObjectId) -> Fallible<()> {
        if current(cx, this)?.is_some() {
            return Ok(());
        }
        let (start, _) = range::bounds_of(cx, range)?;
        if !in_this_document(cx, start.node) {
            return Ok(());
        }
        set_range(cx, this, Some(range), false)
    }

    fn remove_range(cx: &mut Cx<'_>, this: ObjectId, range: ObjectId) -> Fallible<()> {
        if current(cx, this)? != Some(range) {
            return Err(Exception::not_found(
                "The range is not part of the selection.",
            ));
        }
        set_range(cx, this, None, false)
    }

    fn remove_all_ranges(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        set_range(cx, this, None, false)
    }

    fn empty(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        set_range(cx, this, None, false)
    }

    fn collapse(
        cx: &mut Cx<'_>,
        this: ObjectId,
        node: Option<NodeId>,
        offset: u32,
    ) -> Fallible<()> {
        let Some(node) = node else {
            return set_range(cx, this, None, false);
        };
        check_offset(cx, node, offset)?;
        if !in_this_document(cx, node) {
            return Ok(());
        }
        let point = Boundary { node, offset };
        select(cx, this, point, point)
    }

    fn set_position(
        cx: &mut Cx<'_>,
        this: ObjectId,
        node: Option<NodeId>,
        offset: u32,
    ) -> Fallible<()> {
        Self::collapse(cx, this, node, offset)
    }

    fn collapse_to_start(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let Some(range) = current(cx, this)? else {
            return Err(Exception::invalid_state(
                "There is no selection to collapse.",
            ));
        };
        let (start, _) = range::bounds_of(cx, range)?;
        select(cx, this, start, start)
    }

    fn collapse_to_end(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let Some(range) = current(cx, this)? else {
            return Err(Exception::invalid_state(
                "There is no selection to collapse.",
            ));
        };
        let (_, end) = range::bounds_of(cx, range)?;
        select(cx, this, end, end)
    }

    fn extend(cx: &mut Cx<'_>, this: ObjectId, node: NodeId, offset: u32) -> Fallible<()> {
        check_offset(cx, node, offset)?;
        let Some((anchor, _)) = anchor_and_focus(cx, this)? else {
            return Err(Exception::invalid_state("There is no selection to extend."));
        };
        if !in_this_document(cx, node) {
            return Ok(());
        }
        select(cx, this, anchor, Boundary { node, offset })
    }

    fn set_base_and_extent(
        cx: &mut Cx<'_>,
        this: ObjectId,
        anchor_node: NodeId,
        anchor_offset: u32,
        focus_node: NodeId,
        focus_offset: u32,
    ) -> Fallible<()> {
        check_offset(cx, anchor_node, anchor_offset)?;
        check_offset(cx, focus_node, focus_offset)?;
        if !in_this_document(cx, anchor_node) || !in_this_document(cx, focus_node) {
            return Ok(());
        }
        select(
            cx,
            this,
            Boundary {
                node: anchor_node,
                offset: anchor_offset,
            },
            Boundary {
                node: focus_node,
                offset: focus_offset,
            },
        )
    }

    fn select_all_children(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        node::check(cx, node)?;
        if matches!(cx.dom().kind(node), NodeKind::Doctype(_)) {
            return Err(Exception::invalid_node_type(
                "A doctype cannot be selected.",
            ));
        }
        if !in_this_document(cx, node) {
            return Ok(());
        }
        let length = cx.dom().children(node).count() as u32;
        select(
            cx,
            this,
            Boundary { node, offset: 0 },
            Boundary {
                node,
                offset: length,
            },
        )
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

    fn delete_from_document(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        match current(cx, this)? {
            Some(range) => <Web as web::RangeImpl>::delete_contents(cx, range),
            None => Ok(()),
        }
    }

    /// <https://w3c.github.io/selection-api/#dom-selection-containsnode>
    fn contains_node(
        cx: &mut Cx<'_>,
        this: ObjectId,
        node: NodeId,
        allow_partial_containment: bool,
    ) -> Fallible<bool> {
        node::check(cx, node)?;
        let Some(range) = current(cx, this)? else {
            return Ok(false);
        };
        let (start, end) = range::bounds_of(cx, range)?;
        let dom = cx.dom();
        if dom.root_of(node) != dom.root_of(start.node) {
            return Ok(false);
        }
        let first = Boundary { node, offset: 0 };
        let last = Boundary {
            node,
            offset: range::node_length(&dom, node),
        };
        let starts_before =
            |p: Boundary| range::compare_points(&dom, start, p) != Ordering::Greater;
        let ends_after = |p: Boundary| range::compare_points(&dom, p, end) != Ordering::Greater;
        Ok(if allow_partial_containment {
            starts_before(last) && ends_after(first)
        } else {
            starts_before(first) && ends_after(last)
        })
    }

    fn stringify(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        match current(cx, this)? {
            Some(range) => <Web as web::RangeImpl>::stringify(cx, range),
            None => Ok(String::new()),
        }
    }
}
