//! Read-only views of page state for the agent layer: what a control holds
//! now (the user's edits, not the markup's defaults), what is checked and
//! selected, what has focus, and which elements listen for activation.

use catpaw_dom::{Dom, NodeId};
use catpaw_js::EventTargetRef;
use catpaw_style::StyleEngine;

use crate::events::ListenerKind;
use crate::generated as web;
use crate::page::{Cx, PageState};

/// The current value of a text control (`input` of a text type, or
/// `textarea`): the edited value when the user or script changed it, else
/// the default from the markup. `None` for other elements.
pub fn control_value(page: &PageState, el: NodeId) -> Option<String> {
    let value = raw_control_value(page, el)?;
    if !value.is_empty() && page.masked_values.borrow().contains(&el) {
        return Some("***".to_string());
    }
    Some(value)
}

/// Marks a control's value as the user's own (typed during a hand-off):
/// [`control_value`] gives `***` for it until [`unmask_value`].
pub fn mask_value(page: &PageState, el: NodeId) {
    page.masked_values.borrow_mut().insert(el);
}

/// Ends [`mask_value`] for a control the agent itself sets.
pub fn unmask_value(page: &PageState, el: NodeId) {
    page.masked_values.borrow_mut().remove(&el);
}

fn raw_control_value(page: &PageState, el: NodeId) -> Option<String> {
    let dom = page.dom.borrow();
    let element = dom.element(el)?;
    if !element.is_html() {
        return None;
    }
    let local = &*element.name.local;
    if local != "input" && local != "textarea" {
        return None;
    }
    let dirty = page
        .form_state
        .borrow()
        .get(&el)
        .and_then(|s| s.value.clone());
    if let Some(value) = dirty {
        return Some(value);
    }
    Some(if local == "textarea" {
        crate::element::child_text_content(&dom, el)
            .replace("\r\n", "\n")
            .replace('\r', "\n")
    } else {
        element.attr("value").unwrap_or("").to_string()
    })
}

/// Whether a checkbox or radio button is checked now.
pub fn is_checked(page: &PageState, el: NodeId) -> bool {
    let dirty = page.form_state.borrow().get(&el).and_then(|s| s.checked);
    dirty.unwrap_or_else(|| page.dom.borrow().attr(el, "checked").is_some())
}

/// Whether an `option` is selected now.
pub fn is_option_selected(page: &PageState, option: NodeId) -> bool {
    crate::forms::option_selected(page, option)
}

/// The options a `select` shows as selected.
pub fn selected_options(page: &PageState, select: NodeId) -> Vec<NodeId> {
    crate::forms::displayed_options(page, select)
}

/// The focused element, if any (one that left the document is not).
pub fn focused(page: &PageState) -> Option<NodeId> {
    let focused = page.document_state.borrow().focused?;
    let dom = page.dom.borrow();
    (dom.contains(focused) && dom.is_connected(focused)).then_some(focused)
}

/// Event types that make an element something to click.
const ACTIVATION_EVENTS: &[&str] = &[
    "click",
    "mousedown",
    "mouseup",
    "pointerdown",
    "pointerup",
    "touchstart",
];

/// The handler attributes of those events.
const ACTIVATION_HANDLERS: &[&str] = &[
    "onclick",
    "onmousedown",
    "onmouseup",
    "onpointerdown",
    "onpointerup",
    "ontouchstart",
];

/// Whether the element itself has a click-like listener or handler (an
/// `onclick` attribute counts). Listeners delegated to an ancestor
/// (React's root listener) are not seen; the agent layer pairs this with
/// `cursor: pointer`.
pub fn has_activation_listener(page: &PageState, el: NodeId) -> bool {
    {
        let dom = page.dom.borrow();
        if ACTIVATION_HANDLERS
            .iter()
            .any(|name| dom.attr(el, name).is_some())
        {
            return true;
        }
    }
    let listeners = page.listeners.borrow();
    let Some(list) = listeners.get(&EventTargetRef::Node(el)) else {
        return false;
    };
    list.iter().any(|l| {
        !l.removed.get()
            && ACTIVATION_EVENTS.contains(&&*l.type_)
            && match &l.kind {
                ListenerKind::Listener(_) => true,
                ListenerKind::Handler(handler) => handler.is_some(),
            }
    })
}

/// Runs `f` with styles resolved for the document as it is now.
pub fn with_styles<R>(page: &PageState, f: impl FnOnce(&StyleEngine, &Dom) -> R) -> R {
    crate::stylesheets::with_styles(page, f)
}

/// A number that changes whenever what the page shows may have: its
/// document, its style sheets, a scroll position.
pub fn shown_version(page: &PageState) -> u64 {
    crate::layout::geometry_version(page)
}

/// Lets go of the layout kept for a document that is being replaced.
pub fn release_layout(page: &PageState) {
    crate::layout::release(page);
}

/// The viewport size in CSS pixels.
pub fn viewport(page: &PageState) -> (u32, u32) {
    (page.config.viewport_width, page.config.viewport_height)
}

/// The window's scroll position in CSS pixels.
pub fn window_scroll(page: &PageState) -> (f32, f32) {
    crate::layout::window_scroll(page)
}

/// The element at a point of the viewport (CSS pixels), as a click there
/// would hit it.
pub fn element_at(page: &PageState, x: f32, y: f32) -> Option<NodeId> {
    crate::layout::element_from_point(page, x, y)
}

/// Scrolls the element into view (the nearest edge), as an action aimed at
/// it would.
pub fn scroll_into_view(cx: &mut Cx<'_>, el: NodeId) {
    crate::layout::scroll_into_view(
        cx,
        el,
        web::ScrollLogicalPosition::Nearest,
        web::ScrollLogicalPosition::Nearest,
    );
}

/// Scrolls the window by a distance in CSS pixels (clamped to the page).
pub fn scroll_by(cx: &mut Cx<'_>, dx: f32, dy: f32) {
    let (x, y) = crate::layout::window_scroll(cx.page);
    crate::layout::scroll_window_to(cx, x + dx, y + dy);
}

/// Scrolls the content of `el`, or of the nearest element around it that
/// scrolls, by `dy` (the window when none does); how far it moved.
pub fn scroll_within(cx: &mut Cx<'_>, el: NodeId, dy: f32) -> f32 {
    let mut at = Some(el);
    while let Some(node) = at {
        if crate::layout::scrolls_vertically(cx.page, node) {
            let (x, y) = crate::layout::scroll_position(cx.page, node);
            crate::layout::scroll_element_to(cx, node, x, y + dy);
            return crate::layout::scroll_position(cx.page, node).1 - y;
        }
        at = cx.dom().parent_element(node);
    }
    let before = window_scroll(cx.page).1;
    scroll_by(cx, 0.0, dy);
    window_scroll(cx.page).1 - before
}

/// The options of a `select`, in order (those inside `optgroup`s too).
pub fn options_of(page: &PageState, select: NodeId) -> Vec<NodeId> {
    crate::forms::options_of(&page.dom.borrow(), select)
}

/// An option's label (its text, whitespace collapsed) and value.
pub fn option_label_and_value(page: &PageState, option: NodeId) -> (String, String) {
    let dom = page.dom.borrow();
    (
        crate::forms::option_text(&dom, option),
        crate::forms::option_value(&dom, option),
    )
}

/// The element's border box in viewport CSS pixels (x, y, width, height),
/// `None` when it has no box.
pub fn element_rect(page: &PageState, el: NodeId) -> Option<(f32, f32, f32, f32)> {
    let rect = crate::layout::bounding_client_rect(page, el);
    (rect.width > 0.0 || rect.height > 0.0).then_some((rect.x, rect.y, rect.width, rect.height))
}

/// Whether an element is disabled: a disabled form control (a disabled
/// fieldset counts), or `aria-disabled="true"` on it or an ancestor.
pub fn is_disabled(page: &PageState, el: NodeId) -> bool {
    let dom = page.dom.borrow();
    crate::forms::is_disabled(&dom, el)
        || std::iter::once(el).chain(dom.ancestors(el)).any(|n| {
            dom.attr(n, "aria-disabled")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        })
}
