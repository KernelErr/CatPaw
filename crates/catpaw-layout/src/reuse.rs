//! Building a tree again for a document that changed, with what did not
//! change taken from the last tree: the shaped text of inline formatting
//! contexts (see `inline::ShapedCache`), and the layout of subtrees that
//! are laid out on their own.
//!
//! A subtree is carried over when it is the same in both trees (the same
//! boxes for the same nodes with the same computed values, the same
//! inline content) and its root lays out independently of what is around
//! it: a flex or grid item, an atomic inline box, a positioned box, the
//! root. Such a subtree keeps its boxes' layouts and Taffy's caches, which
//! answer for the inputs the root had last time; if the root is asked for
//! a layout with other inputs, the caches of the whole subtree are dropped
//! and it is laid out afresh (see `LayoutTree::forget_carried_over`).
//! In-flow blocks are never carried over by themselves, as floats around
//! them may have moved.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use style::servo_arc::Arc;

use crate::inline::ShapedCache;
use crate::{BoxId, BoxKind, LayoutTree, Positioning};

/// What a new tree can take from the last one.
pub(crate) struct Reuse {
    /// The shaped text of the last tree's inline contexts.
    pub shaped: ShapedCache,
    /// The last tree, without its inline contexts (they are in `shaped`).
    old: LayoutTree,
    /// The last tree's boxes by fingerprint; `None` where several shared
    /// one.
    by_fingerprint: HashMap<u64, Option<BoxId>>,
}

/// The boxes laid out from the top: the root, and those positioned
/// against the initial containing block.
fn tops(tree: &LayoutTree) -> Vec<BoxId> {
    tree.root
        .into_iter()
        .chain(tree.oof_root.iter().copied())
        .collect()
}

/// Fills in every box's fingerprint: what it and its subtree are laid out
/// from, hashed. Computed bottom-up.
pub(crate) fn fingerprint(tree: &mut LayoutTree) {
    for top in tops(tree) {
        let mut stack = vec![(top, false)];
        while let Some((id, children_done)) = stack.pop() {
            if children_done {
                let fingerprint = fingerprint_of(tree, id);
                tree.boxes[id].fingerprint = fingerprint;
                continue;
            }
            stack.push((id, true));
            stack.extend(tree.boxes[id].children.iter().map(|&c| (c, false)));
        }
    }
}

fn fingerprint_of(tree: &LayoutTree, id: BoxId) -> u64 {
    let b = &tree.boxes[id];
    let mut hasher = DefaultHasher::new();
    b.node
        .map(|n| slotmap::Key::data(&n).as_ffi())
        .hash(&mut hasher);
    b.kind.hash(&mut hasher);
    b.positioning.hash(&mut hasher);
    (b.style.heap_ptr() as usize).hash(&mut hasher);
    let i = &b.intrinsic;
    for value in [i.width, i.height, i.ratio] {
        value.map(f32::to_bits).hash(&mut hasher);
    }
    i.default_width.to_bits().hash(&mut hasher);
    i.default_height.to_bits().hash(&mut hasher);
    b.inline.as_ref().map(|c| c.ops.hash).hash(&mut hasher);
    b.children.len().hash(&mut hasher);
    for &child in &b.children {
        tree.boxes[child].fingerprint.hash(&mut hasher);
    }
    hasher.finish()
}

/// Whether two boxes are the same box for layout: same node, kind,
/// positioning, computed values, intrinsic size and inline content (the
/// new box's shaped text came from the old box).
fn same_box(new: &LayoutTree, n: BoxId, old: &LayoutTree, o: BoxId) -> bool {
    let (a, b) = (&new.boxes[n], &old.boxes[o]);
    a.fingerprint == b.fingerprint
        && a.node == b.node
        && a.kind == b.kind
        && a.positioning == b.positioning
        && Arc::ptr_eq(&a.style, &b.style)
        && a.intrinsic == b.intrinsic
        && a.children.len() == b.children.len()
        && match &a.inline {
            Some(context) => context.reused_from == Some(o),
            None => a.kind != BoxKind::InlineRoot,
        }
}

/// Whether the subtrees of `n` in the new tree and `o` in the old one are
/// the same, box for box.
fn same_subtree(new: &LayoutTree, n: BoxId, old: &LayoutTree, o: BoxId) -> bool {
    let mut stack = vec![(n, o)];
    while let Some((n, o)) = stack.pop() {
        if !same_box(new, n, old, o) {
            return false;
        }
        stack.extend(
            new.boxes[n]
                .children
                .iter()
                .copied()
                .zip(old.boxes[o].children.iter().copied()),
        );
    }
    true
}

/// Counts, once the tree is laid out, what it took from the last one: the
/// inline contexts that kept their shaped text, and the boxes whose
/// carried-over layout was kept.
pub(crate) fn count_reused(tree: &mut LayoutTree) {
    let mut reused = crate::Reused::default();
    for b in tree.boxes.values() {
        if let Some(context) = &b.inline {
            if context.reused_from.is_some() {
                reused.shaped += 1;
            } else {
                reused.reshaped += 1;
            }
        }
    }
    let roots: Vec<BoxId> = tree
        .boxes
        .iter()
        .filter(|(_, b)| b.transplanted)
        .map(|(id, _)| id)
        .collect();
    for root in roots {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            reused.laid_out += 1;
            stack.extend(tree.boxes[id].children.iter().copied());
        }
    }
    tree.reused = reused;
}

impl Reuse {
    pub(crate) fn new(mut old: LayoutTree) -> Self {
        let mut shaped = ShapedCache::default();
        let mut by_fingerprint: HashMap<u64, Option<BoxId>> = HashMap::new();
        for (id, b) in old.boxes.iter_mut() {
            if let Some(context) = b.inline.take() {
                shaped.insert(id, context);
            }
            by_fingerprint
                .entry(b.fingerprint)
                .and_modify(|found| *found = None)
                .or_insert(Some(id));
        }
        Self {
            shaped,
            old,
            by_fingerprint,
        }
    }

    /// Carries over the layout of the subtrees of `tree` (built, with
    /// fingerprints) that the last tree had too and that lay out on their
    /// own.
    pub(crate) fn transplant(self, tree: &mut LayoutTree) {
        let mut stack: Vec<(BoxId, bool)> = tops(tree).into_iter().map(|id| (id, true)).collect();
        while let Some((id, independent)) = stack.pop() {
            if independent
                && let Some(&Some(old)) = self.by_fingerprint.get(&tree.boxes[id].fingerprint)
                && same_subtree(tree, id, &self.old, old)
            {
                self.copy_subtree(tree, id, old);
                tree.boxes[id].transplanted = true;
                continue;
            }
            let kind = tree.boxes[id].kind;
            let lays_out_children_alone =
                matches!(kind, BoxKind::Flex | BoxKind::Grid | BoxKind::InlineRoot);
            for &child in &tree.boxes[id].children {
                let positioned = matches!(
                    tree.boxes[child].positioning,
                    Positioning::Absolute | Positioning::Fixed
                );
                stack.push((child, lays_out_children_alone || positioned));
            }
        }
    }

    /// Copies the layout and the Taffy cache of every box of the old
    /// subtree `o` to its counterpart in the new subtree `n`.
    fn copy_subtree(&self, tree: &mut LayoutTree, n: BoxId, o: BoxId) {
        let mut stack = vec![(n, o)];
        while let Some((n, o)) = stack.pop() {
            let old = &self.old.boxes[o];
            let new = &mut tree.boxes[n];
            new.layout = old.layout;
            new.cache = old.cache.clone();
            stack.extend(
                new.children
                    .iter()
                    .copied()
                    .zip(old.children.iter().copied()),
            );
        }
    }
}
