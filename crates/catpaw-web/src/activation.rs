//! Activation: what a click does beyond dispatching the event.
//!
//! Only the parts scripts commonly rely on synchronously are here (checkbox
//! and radio state). Navigation, form submission and the rest of the
//! activation behaviors arrive with user input in the interaction milestone.

use catpaw_dom::NodeId;
use catpaw_js::EventTargetRef;

use crate::element;
use crate::events;
use crate::page::Cx;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Toggle {
    Checkbox,
    Radio,
}

fn classify(cx: &Cx<'_>, el: NodeId) -> (bool, Option<Toggle>) {
    let dom = cx.dom();
    let Some(data) = dom.element(el).filter(|e| e.is_html()) else {
        return (false, None);
    };
    let local = &*data.name.local;
    let disabled =
        matches!(local, "button" | "input" | "select" | "textarea") && data.has_attr("disabled");
    let toggle = if local == "input" {
        match data.attr("type").map(str::to_ascii_lowercase).as_deref() {
            Some("checkbox") => Some(Toggle::Checkbox),
            Some("radio") => Some(Toggle::Radio),
            _ => None,
        }
    } else {
        None
    };
    (disabled, toggle)
}

/// Dispatches a `click` at `el` and runs its activation behavior. `trusted`
/// distinguishes user input from `element.click()`.
pub fn click(cx: &mut Cx<'_>, el: NodeId, trusted: bool) {
    let (disabled, toggle) = classify(cx, el);
    if disabled {
        return;
    }

    // Legacy-pre-activation: the new state is visible to click listeners,
    // and rolled back if one of them cancels the event.
    let previous = toggle.map(|kind| {
        let was = element::is_checked(cx, el);
        element::set_checked(cx, el, kind == Toggle::Radio || !was);
        was
    });

    let event = crate::ui_events::synthetic_click(cx, trusted);
    let proceed = events::dispatch(cx, EventTargetRef::Node(el), event);

    if let Some(was) = previous {
        if !proceed {
            element::set_checked(cx, el, was);
        } else if element::is_checked(cx, el) != was {
            events::fire(cx, EventTargetRef::Node(el), "input", true, false);
            events::fire(cx, EventTargetRef::Node(el), "change", true, false);
        }
    }
}
