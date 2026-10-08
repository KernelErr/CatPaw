//! The page's layout: built from the current styles when something asks
//! for geometry, kept until a scroll position changes or the style sheets
//! or the document change in a way that shows (the arena's journal and
//! the style engine say which changes do).
//!
//! Boxes live in document coordinates; the CSSOM View answers in viewport
//! coordinates, which subtract the window's scroll position (except for
//! fixed boxes, which the layout already keeps viewport-relative).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use catpaw_dom::QuirksMode;
use catpaw_dom::{Change, Dom, NodeId, NodeKind};
use catpaw_js::{EventTargetRef, ObjectId};
use catpaw_layout::{BuildInput, LayoutTree, Rect, Viewport};
use catpaw_style::StyleEngine;

use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{element, events, stylesheets};

/// The page's layout state.
#[derive(Default)]
pub(crate) struct Layouts {
    tree: RefCell<Option<LayoutTree>>,
    /// The DOM version, CSSOM edit count and scroll version the tree is
    /// known to be up to date with: it was built then, or nothing that
    /// changed since matters to it.
    current: Cell<Option<(u64, u64, u64)>>,
    /// Scroll positions of scroll containers other than the viewport.
    scroll_offsets: RefCell<HashMap<NodeId, (f32, f32)>>,
    /// Bumped whenever an element's scroll position changes.
    scroll_version: Cell<u64>,
    /// How many times a tree was built.
    builds: Cell<u64>,
}

/// Lets go of the layout kept for a document that is being replaced, so
/// that it does not stay alive while the next one loads.
pub(crate) fn release(page: &PageState) {
    *page.layouts.tree.borrow_mut() = None;
    page.layouts.current.set(None);
}

/// Whether `CATPAW_RENDER_STATS` asks for each step of rendering work
/// (restyles, layouts, index rebuilds) to be logged to stderr with its
/// duration, for profiling.
fn stats_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CATPAW_RENDER_STATS").is_some_and(|v| v != "0"))
}

/// Logs a step of rendering work begun at `started`, when asked to.
pub(crate) fn log_step(what: &str, started: Instant) {
    if stats_enabled() {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        eprintln!("[render] {what} {ms:.2}ms");
    }
}

/// Runs `f` with a layout that reflects the document as it is now. The
/// tree is built again only when something it shows may have changed:
/// a computed style, a rendered part of the tree or of its text, what
/// sizes a replaced element, a scroll position.
pub(crate) fn with_layout<R>(page: &PageState, f: impl FnOnce(&LayoutTree, &Dom) -> R) -> R {
    stylesheets::with_engine(page, |engine, dom| {
        let stamp = (
            dom.version(),
            page.styles.edits.get(),
            page.layouts.scroll_version.get(),
        );
        let current = page.layouts.current.get();
        if current != Some(stamp) || page.layouts.tree.borrow().is_none() {
            stylesheets::restyle(engine, dom);
            let restyled = engine.take_restyled();
            let still_valid = match (current, page.layouts.tree.borrow().as_ref()) {
                (Some((since, _, scrolled)), Some(tree)) => {
                    scrolled == stamp.2
                        && !restyled.full
                        && !restyled
                            .elements
                            .iter()
                            .any(|&el| shown(tree, engine, dom, el))
                        && !layout_affected(tree, engine, dom, since)
                }
                _ => false,
            };
            if !still_valid {
                let started = Instant::now();
                let fonts = catpaw_text::shared_fonts();
                let scroll_offsets = page.layouts.scroll_offsets.borrow();
                let input = BuildInput {
                    dom,
                    styles: engine,
                    fonts: &fonts,
                    viewport: viewport(page),
                    scroll_offsets: &scroll_offsets,
                };
                // What did not change is taken from the last tree.
                let previous = page.layouts.tree.borrow_mut().take();
                let tree = match previous {
                    Some(previous) => LayoutTree::rebuild(previous, input),
                    None => LayoutTree::build(input),
                };
                drop(scroll_offsets);
                let reused = tree.reused();
                *page.layouts.tree.borrow_mut() = Some(tree);
                page.layouts.builds.set(page.layouts.builds.get() + 1);
                log_step("layout", started);
                if stats_enabled() {
                    eprintln!(
                        "[render] layout-reuse text-shaped={} text-kept={} boxes-kept={}",
                        reused.reshaped, reused.shaped, reused.laid_out
                    );
                }
            }
            page.layouts.current.set(Some(stamp));
        }
        let tree = page.layouts.tree.borrow();
        f(tree.as_ref().expect("layout was just built"), dom)
    })
}

/// Whether an element is shown in the tree, or would be now.
fn shown(tree: &LayoutTree, engine: &StyleEngine, dom: &Dom, el: NodeId) -> bool {
    tree.box_of(el).is_some()
        || tree.inline_style(el).is_some()
        || (dom.contains(el) && dom.is_connected(el) && !engine.is_display_none(dom, el))
}

/// Whether the changes to the document since it was at `since` can have
/// changed the tree, other than through the computed styles (which the
/// style engine reports): children or text of something rendered, or the
/// attributes a replaced element is sized by, or which children a shadow
/// root's slots take.
fn layout_affected(tree: &LayoutTree, engine: &StyleEngine, dom: &Dom, since: u64) -> bool {
    let Some(changes) = dom.changes_since(since) else {
        return true;
    };
    // Whether what is inside `node` may be rendered: an element not in a
    // `display: none` subtree, a document or a shadow root.
    let renders_content = |node: NodeId| match dom.kind(node) {
        NodeKind::Element(_) => !engine.is_display_none(dom, node),
        _ => true,
    };
    // A select is sized by the text of its options, which are not shown.
    let in_shown_select = |node: NodeId| {
        std::iter::once(node)
            .chain(dom.ancestors(node))
            .any(|a| dom.is_html_element(a, "select") && shown(tree, engine, dom, a))
    };
    changes.iter().any(|change| match *change {
        Change::Inserted { parent, .. } | Change::Removed { parent, .. } => {
            dom.contains(parent)
                && dom.is_connected(parent)
                && (renders_content(parent) || in_shown_select(parent))
        }
        Change::Data(node) => {
            if !dom.contains(node) || !dom.is_connected(node) {
                return false;
            }
            match dom.kind(node) {
                NodeKind::Text(_) => dom
                    .parent(node)
                    .is_some_and(|p| renders_content(p) || in_shown_select(p)),
                NodeKind::Element(el) => {
                    let slotting = el.shadow_root.is_some()
                        || dom.is_html_element(node, "slot")
                        || dom
                            .parent_element(node)
                            .and_then(|p| dom.element(p))
                            .is_some_and(|p| p.shadow_root.is_some());
                    (catpaw_layout::is_replaced(dom, node) && shown(tree, engine, dom, node))
                        || (slotting && renders_content(node))
                }
                // A document's URL or quirks mode (the latter restyles
                // everything), comments, doctypes: nothing rendered.
                _ => false,
            }
        }
        Change::Freed(_) => false,
    })
}

/// A number that changes whenever geometry may have: the document, the
/// style sheets, or a scroll position (an element's or the window's).
pub(crate) fn geometry_version(page: &PageState) -> u64 {
    let (sx, sy) = window_scroll(page);
    let mut hash = page.dom.borrow().version();
    for part in [
        page.styles.edits.get(),
        page.layouts.scroll_version.get(),
        u64::from(sx.to_bits()),
        u64::from(sy.to_bits()),
    ] {
        hash = hash.wrapping_mul(0x100_0000_01b3).wrapping_add(part);
    }
    hash
}

pub(crate) fn viewport(page: &PageState) -> Viewport {
    Viewport {
        width: page.config.viewport_width as f32,
        height: page.config.viewport_height as f32,
    }
}

/// The window's scroll position.
pub(crate) fn window_scroll(page: &PageState) -> (f32, f32) {
    let state = page.document_state.borrow();
    (state.scroll_x as f32, state.scroll_y as f32)
}

/// A document rectangle as the viewport sees it.
fn to_viewport(page: &PageState, rect: Rect, fixed: bool) -> Rect {
    if fixed {
        rect
    } else {
        let (sx, sy) = window_scroll(page);
        rect.translate(-sx, -sy)
    }
}

/// Whether the node is rendered inside a fixed box.
fn in_fixed(tree: &LayoutTree, node: NodeId) -> bool {
    tree.box_of(node)
        .and_then(|id| tree.fixed_ancestor(id))
        .is_some()
}

/// The rectangles of a node in viewport coordinates, one per fragment.
pub(crate) fn client_rects(page: &PageState, node: NodeId) -> Vec<Rect> {
    with_layout(page, |tree, dom| {
        let fixed = in_fixed(tree, node);
        tree.node_rects(dom, node)
            .into_iter()
            .map(|r| to_viewport(page, r, fixed))
            .collect()
    })
}

/// The bounding rectangle of a node in viewport coordinates; empty when it
/// is not rendered.
pub(crate) fn bounding_client_rect(page: &PageState, node: NodeId) -> Rect {
    client_rects(page, node)
        .into_iter()
        .reduce(|a, b| a.union(&b))
        .unwrap_or_default()
}

/// The root element, body and quirks mode of the node's document.
fn document_parts(dom: &Dom, node: NodeId) -> (Option<NodeId>, Option<NodeId>, bool) {
    let document = dom.owner_document(node);
    let root = dom.child_elements(document).next();
    let body = root.and_then(|r| {
        dom.child_elements(r)
            .find(|e| dom.is_html_element(*e, "body"))
    });
    let quirks = dom
        .document_data_of(node)
        .is_some_and(|d| d.quirks_mode == QuirksMode::Quirks);
    (root, body, quirks)
}

/// Whether the element stands for the viewport in the CSSOM View sense:
/// the root element, or the body in quirks mode.
fn is_viewport_element(dom: &Dom, node: NodeId) -> bool {
    let (root, body, quirks) = document_parts(dom, node);
    Some(node) == root || (quirks && Some(node) == body)
}

/// The scroll geometry of the document: how far it extends and the
/// viewport it scrolls in.
fn document_scroll_size(page: &PageState, tree: &LayoutTree, dom: &Dom) -> (f32, f32) {
    let viewport = viewport(page);
    let mut width = viewport.width;
    let mut height = viewport.height;
    if let Some(root) = tree.root() {
        let metrics = tree.scroll_metrics(root);
        let rect = tree.border_box(root);
        width = width.max(metrics.scroll_width.max(rect.right()));
        height = height.max(metrics.scroll_height.max(rect.bottom()));
        // The body's margins count towards the document's extent.
        let (_, body, _) = document_parts(
            dom,
            tree.root()
                .and_then(|r| tree.get(r).node)
                .unwrap_or(dom.document()),
        );
        if let Some(body) = body
            && let Some(id) = tree.box_of(body)
        {
            let b = tree.border_box(id);
            let m = tree.scroll_metrics(id);
            height = height.max(b.bottom().max(b.y + m.scroll_height));
            width = width.max(b.right().max(b.x + m.scroll_width));
        }
    }
    (width, height)
}

/// `clientWidth`/`clientHeight`, `clientTop`/`clientLeft`.
pub(crate) fn client_box(page: &PageState, node: NodeId) -> Rect {
    with_layout(page, |tree, dom| {
        if is_viewport_element(dom, node) {
            let v = viewport(page);
            return Rect::new(0.0, 0.0, v.width, v.height);
        }
        match tree.box_of(node) {
            Some(id) => {
                let metrics = tree.scroll_metrics(id);
                let border = tree.border_box(id);
                Rect::new(
                    metrics.client.x - border.x,
                    metrics.client.y - border.y,
                    metrics.client.width,
                    metrics.client.height,
                )
            }
            None => Rect::default(),
        }
    })
}

/// `scrollWidth`/`scrollHeight`.
pub(crate) fn scroll_size(page: &PageState, node: NodeId) -> (f32, f32) {
    with_layout(page, |tree, dom| {
        if is_viewport_element(dom, node) {
            return document_scroll_size(page, tree, dom);
        }
        match tree.box_of(node) {
            Some(id) => {
                let m = tree.scroll_metrics(id);
                (m.scroll_width, m.scroll_height)
            }
            None => (0.0, 0.0),
        }
    })
}

/// Whether an element scrolls its content up and down (a scroll container
/// with more content than room).
pub(crate) fn scrolls_vertically(page: &PageState, node: NodeId) -> bool {
    with_layout(page, |tree, dom| {
        !is_viewport_element(dom, node)
            && tree.box_of(node).is_some_and(|id| {
                let m = tree.scroll_metrics(id);
                tree.is_scroll_container(id) && m.scroll_height > m.client.height + 0.5
            })
    })
}

/// `scrollLeft`/`scrollTop`.
pub(crate) fn scroll_position(page: &PageState, node: NodeId) -> (f32, f32) {
    let is_viewport = with_layout(page, |_, dom| is_viewport_element(dom, node));
    if is_viewport {
        return window_scroll(page);
    }
    page.layouts
        .scroll_offsets
        .borrow()
        .get(&node)
        .copied()
        .unwrap_or((0.0, 0.0))
}

/// Scrolls an element (or, for the viewport element, the window) to a
/// position, clamped to what its content allows, and queues the `scroll`
/// event if anything moved.
pub(crate) fn scroll_element_to(cx: &mut Cx<'_>, node: NodeId, x: f32, y: f32) {
    let page = cx.page;
    let is_viewport = with_layout(page, |_, dom| is_viewport_element(dom, node));
    if is_viewport {
        scroll_window_to(cx, x, y);
        return;
    }
    let (max_x, max_y, is_scroll_container) =
        with_layout(page, |tree, _| match tree.box_of(node) {
            Some(id) => {
                let m = tree.scroll_metrics(id);
                (
                    (m.scroll_width - m.client.width).max(0.0),
                    (m.scroll_height - m.client.height).max(0.0),
                    tree.is_scroll_container(id),
                )
            }
            None => (0.0, 0.0, false),
        });
    if !is_scroll_container {
        return;
    }
    let target = (x.clamp(0.0, max_x), y.clamp(0.0, max_y));
    let previous = page
        .layouts
        .scroll_offsets
        .borrow()
        .get(&node)
        .copied()
        .unwrap_or((0.0, 0.0));
    if target == previous {
        return;
    }
    page.layouts
        .scroll_offsets
        .borrow_mut()
        .insert(node, target);
    page.layouts
        .scroll_version
        .set(page.layouts.scroll_version.get() + 1);
    crate::event_loop::queue_task(page, "scroll event", move |cx| {
        events::fire(cx, EventTargetRef::Node(node), "scroll", false, false);
    });
}

/// Scrolls the window, clamped to the document's extent, and queues the
/// `scroll` event on the document if anything moved.
pub(crate) fn scroll_window_to(cx: &mut Cx<'_>, x: f32, y: f32) {
    let page = cx.page;
    let (width, height) = with_layout(page, |tree, dom| document_scroll_size(page, tree, dom));
    let viewport = viewport(page);
    let target = (
        x.clamp(0.0, (width - viewport.width).max(0.0)),
        y.clamp(0.0, (height - viewport.height).max(0.0)),
    );
    let previous = window_scroll(page);
    if target == previous {
        return;
    }
    {
        let mut state = page.document_state.borrow_mut();
        state.scroll_x = f64::from(target.0);
        state.scroll_y = f64::from(target.1);
    }
    let document = page.document();
    crate::event_loop::queue_task(page, "scroll event", move |cx| {
        events::fire(cx, EventTargetRef::Node(document), "scroll", true, false);
    });
}

/// `scrollIntoView`: scrolls the nearest scroll container and the window
/// until the element shows at the asked edge.
pub(crate) fn scroll_into_view(
    cx: &mut Cx<'_>,
    node: NodeId,
    block: web::ScrollLogicalPosition,
    inline: web::ScrollLogicalPosition,
) {
    let page = cx.page;
    let (rect, container) = with_layout(page, |tree, dom| {
        let rect = tree.bounding_rect(dom, node);
        let container = dom.ancestors(node).find(|a| {
            tree.box_of(*a)
                .is_some_and(|id| tree.is_scroll_container(id))
        });
        (rect, container)
    });
    let Some(mut rect) = rect else {
        return;
    };
    fn aligned(
        position: web::ScrollLogicalPosition,
        start: f32,
        size: f32,
        view_start: f32,
        view_size: f32,
    ) -> f32 {
        match position {
            web::ScrollLogicalPosition::Start => start - view_start,
            web::ScrollLogicalPosition::End => start + size - view_start - view_size,
            web::ScrollLogicalPosition::Center => start + size / 2.0 - view_start - view_size / 2.0,
            web::ScrollLogicalPosition::Nearest => {
                if start < view_start {
                    start - view_start
                } else if start + size > view_start + view_size {
                    (start + size - view_start - view_size).min(start - view_start)
                } else {
                    0.0
                }
            }
        }
    }
    if let Some(container) = container
        && let Some((client, current)) = with_layout(page, |tree, _| {
            tree.box_of(container).map(|id| {
                (
                    tree.scroll_metrics(id).client,
                    page.layouts
                        .scroll_offsets
                        .borrow()
                        .get(&container)
                        .copied()
                        .unwrap_or((0.0, 0.0)),
                )
            })
        })
    {
        let dx = aligned(inline, rect.x, rect.width, client.x, client.width);
        let dy = aligned(block, rect.y, rect.height, client.y, client.height);
        scroll_element_to(cx, container, current.0 + dx, current.1 + dy);
        rect = rect.translate(-dx, -dy);
    }
    let viewport = viewport(page);
    let (sx, sy) = window_scroll(page);
    let dx = aligned(inline, rect.x, rect.width, sx, viewport.width);
    let dy = aligned(block, rect.y, rect.height, sy, viewport.height);
    scroll_window_to(cx, sx + dx, sy + dy);
}

/// `offsetParent`, `offsetTop`, `offsetLeft`, `offsetWidth`, `offsetHeight`.
pub(crate) struct Offsets {
    pub parent: Option<NodeId>,
    pub top: f32,
    pub left: f32,
    pub width: f32,
    pub height: f32,
}

pub(crate) fn offsets(page: &PageState, node: NodeId) -> Offsets {
    with_layout(page, |tree, dom| {
        let Some(rect) = tree.bounding_rect(dom, node) else {
            return Offsets {
                parent: None,
                top: 0.0,
                left: 0.0,
                width: 0.0,
                height: 0.0,
            };
        };
        let parent = tree.offset_parent(dom, node);
        let (_, body, _) = document_parts(dom, node);
        // Against the body, offsets are measured from the initial
        // containing block rather than the body's padding edge.
        let (origin_x, origin_y) = parent
            .filter(|p| Some(*p) != body)
            .and_then(|p| tree.box_of(p))
            .map(|id| {
                let padding = tree.padding_box(id);
                (padding.x, padding.y)
            })
            .unwrap_or((0.0, 0.0));
        Offsets {
            parent,
            top: rect.y - origin_y,
            left: rect.x - origin_x,
            width: rect.width,
            height: rect.height,
        }
    })
}

/// `elementFromPoint`: viewport coordinates in, the topmost element out.
pub(crate) fn element_from_point(page: &PageState, x: f32, y: f32) -> Option<NodeId> {
    let viewport = viewport(page);
    if x < 0.0 || y < 0.0 || x >= viewport.width || y >= viewport.height {
        return None;
    }
    let (sx, sy) = window_scroll(page);
    with_layout(page, |tree, dom| {
        tree.hit_test(dom, x + sx, y + sy, (sx, sy))
            .map(|hit| hit.element)
    })
}

/// `elementsFromPoint`: the element under the point and the ancestors
/// whose boxes contain it, nearest first.
pub(crate) fn elements_from_point(page: &PageState, x: f32, y: f32) -> Vec<NodeId> {
    let Some(first) = element_from_point(page, x, y) else {
        return Vec::new();
    };
    with_layout(page, |tree, dom| {
        let (sx, sy) = window_scroll(page);
        let mut out = vec![first];
        for ancestor in dom.ancestors(first) {
            if !dom.is_element(ancestor) {
                continue;
            }
            let contains = tree.bounding_rect(dom, ancestor).is_some_and(|r| {
                let fixed = in_fixed(tree, ancestor);
                let r = if fixed { r.translate(sx, sy) } else { r };
                r.contains(x + sx, y + sy)
            });
            let is_root = Some(ancestor) == tree.root().and_then(|r| tree.get(r).node);
            if contains || is_root {
                out.push(ancestor);
            }
        }
        out
    })
}

/// A PNG of the page as laid out now: the viewport at its scroll position,
/// or the whole document.
pub fn screenshot(page: &PageState, full_page: bool) -> Vec<u8> {
    with_layout(page, |tree, dom| {
        let viewport = viewport(page);
        let (width, height, scroll) = if full_page {
            let (w, h) = document_scroll_size(page, tree, dom);
            // Bounded, as browsers bound their screenshots.
            (
                w.clamp(1.0, 16384.0).ceil() as u32,
                h.clamp(1.0, 16384.0).ceil() as u32,
                (0.0, 0.0),
            )
        } else {
            (
                viewport.width.max(1.0) as u32,
                viewport.height.max(1.0) as u32,
                window_scroll(page),
            )
        };
        catpaw_paint::render_png_with(
            tree,
            dom,
            &catpaw_paint::Options {
                width,
                height,
                scroll,
                scale: page.config.device_pixel_ratio as f32,
            },
            &|node| page.canvases.pixmap(node),
        )
    })
}

/// A `DOMRectList`.
pub struct RectListObject {
    pub rects: Vec<Rect>,
}
crate::platform_object!(RectListObject, DOMRectList);

pub(crate) fn rect_list(page: &PageState, rects: Vec<Rect>) -> ObjectId {
    page.alloc(RectListObject { rects })
}

pub(crate) fn rect_object(page: &PageState, rect: Rect) -> ObjectId {
    page.alloc(element::RectObject {
        iface: InterfaceId::DOMRect,
        x: f64::from(rect.x),
        y: f64::from(rect.y),
        width: f64::from(rect.width),
        height: f64::from(rect.height),
    })
}

impl web::DOMRectListImpl for crate::Web {
    fn length(cx: &mut Cx<'_>, this: ObjectId) -> catpaw_js::Fallible<u32> {
        cx.page
            .with::<RectListObject, _>(this, |l| l.rects.len() as u32)
    }

    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> catpaw_js::Fallible<Option<ObjectId>> {
        let rect = cx
            .page
            .with::<RectListObject, _>(this, |l| l.rects.get(index as usize).copied())?;
        Ok(rect.map(|r| rect_object(cx.page, r)))
    }

    fn indexed_get(
        cx: &mut Cx<'_>,
        this: ObjectId,
        index: u32,
    ) -> catpaw_js::Fallible<Option<ObjectId>> {
        <Self as web::DOMRectListImpl>::item(cx, this, index)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use catpaw_dom::{Attr, LocalName, QualName, ns};
    use url::Url;

    use super::*;
    use crate::page::PageConfig;

    /// A page whose document is `html`, without script.
    pub(crate) fn page_with(html: &str) -> PageState {
        let page = PageState::new(
            Url::parse("https://example.test/dir/page.html").unwrap(),
            PageConfig {
                viewport_width: 800,
                viewport_height: 600,
                ..PageConfig::default()
            },
        );
        let document = page.dom.borrow().document();
        catpaw_dom::parse_document_into(&page.dom, document, html, &Default::default());
        page
    }

    pub(crate) fn find(page: &PageState, id: &str) -> NodeId {
        let dom = page.dom.borrow();
        dom.descendants(dom.document())
            .find(|&n| dom.attr(n, "id") == Some(id))
            .unwrap()
    }

    pub(crate) fn set_attr(page: &PageState, el: NodeId, local: &str, value: &str) {
        page.dom
            .borrow_mut()
            .element_mut(el)
            .unwrap()
            .set_attr(QualName::new(None, ns!(), LocalName::from(local)), value);
    }

    fn height(page: &PageState, id: &str) -> f32 {
        bounding_client_rect(page, find(page, id)).height
    }

    fn builds(page: &PageState) -> u64 {
        page.layouts.builds.get()
    }

    /// The first element of the document with this local name.
    fn first(page: &PageState, local: &str) -> NodeId {
        let dom = page.dom.borrow();
        dom.descendants(dom.document())
            .find(|&n| dom.is_html_element(n, local))
            .unwrap()
    }

    const DOCUMENT: &str = r#"<!doctype html><html><head><title>t</title></head>
        <body style="margin:0"><div id=a style="height:50px"></div>
        <p id=p>text</p><img id=img width=10 height=20><div id=hidden hidden><b id=b>x</b></div>"#;

    #[test]
    fn writes_nothing_shown_depends_on_keep_the_layout() {
        let page = page_with(DOCUMENT);
        assert_eq!(height(&page, "a"), 50.0);
        assert_eq!(builds(&page), 1);
        let restyles = stylesheets::restyle_counts(&page);

        // Bookkeeping does not even move the version.
        let version = page.dom.borrow().version();
        let a = find(&page, "a");
        page.dom.borrow_mut().set_script_already_started(a, true);
        page.dom
            .borrow_mut()
            .set_custom_element_state(a, catpaw_dom::CustomElementState::Failed);
        assert_eq!(page.dom.borrow().version(), version);
        assert_eq!(height(&page, "a"), 50.0);

        // An attribute no selector reads, the same value again, the title,
        // a script in the head, what is inside `display: none`: the tree
        // stays.
        set_attr(&page, a, "data-x", "1");
        set_attr(&page, a, "style", "height:50px");
        let title = first(&page, "title");
        {
            let mut dom = page.dom.borrow_mut();
            let text = dom.first_child(title).unwrap();
            dom.node_mut(text).kind = NodeKind::Text("another title".into());
            let head = dom.parent(title).unwrap();
            let script = dom.create_html_element("script", vec![Attr::html("src", "x.js")]);
            dom.append_child(head, script);
        }
        set_attr(&page, find(&page, "b"), "class", "y");
        assert_eq!(height(&page, "a"), 50.0);
        assert_eq!(builds(&page), 1, "nothing shown changed");
        assert_eq!(
            stylesheets::restyle_counts(&page).0,
            restyles.0,
            "no restyle from scratch"
        );
    }

    #[test]
    fn writes_that_show_differently_rebuild_the_layout() {
        let page = page_with(DOCUMENT);
        assert_eq!(height(&page, "a"), 50.0);
        assert_eq!(builds(&page), 1);
        let restyles = stylesheets::restyle_counts(&page).0;

        // A style attribute.
        set_attr(&page, find(&page, "a"), "style", "height:70px");
        assert_eq!(height(&page, "a"), 70.0);
        assert_eq!(builds(&page), 2);

        // The attributes a replaced element is sized by.
        set_attr(&page, find(&page, "img"), "height", "30");
        assert_eq!(height(&page, "img"), 30.0);
        assert_eq!(builds(&page), 3);

        // Text that is shown.
        let line = height(&page, "p");
        let p = find(&page, "p");
        {
            let mut dom = page.dom.borrow_mut();
            let text = dom.first_child(p).unwrap();
            dom.node_mut(text).kind = NodeKind::Text("text ".repeat(400));
        }
        assert!(height(&page, "p") > line);
        assert_eq!(builds(&page), 4);

        // Something shown again.
        let hidden = find(&page, "hidden");
        page.dom
            .borrow_mut()
            .element_mut(hidden)
            .unwrap()
            .remove_attr("hidden");
        assert!(height(&page, "b") > 0.0);
        assert_eq!(builds(&page), 5);

        // A new element.
        let body = first(&page, "body");
        {
            let mut dom = page.dom.borrow_mut();
            let div = dom.create_html_element(
                "div",
                vec![Attr::html("id", "new"), Attr::html("style", "height:5px")],
            );
            dom.append_child(body, div);
        }
        assert_eq!(height(&page, "new"), 5.0);
        assert_eq!(builds(&page), 6);
        assert_eq!(
            stylesheets::restyle_counts(&page).0,
            restyles,
            "every restyle was incremental"
        );
    }
}
