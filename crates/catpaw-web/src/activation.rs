//! Activation: what a click does beyond dispatching the event
//! (<https://html.spec.whatwg.org/multipage/interaction.html#activation>).
//!
//! Checkboxes and radio buttons toggle before the event and roll back if it
//! is cancelled; links navigate, submit and reset buttons act on their
//! form, labels pass the click on to their control, and `summary` opens
//! or closes its `details`.

use catpaw_dom::{Dom, NodeId};
use catpaw_js::EventTargetRef;

use crate::element;
use crate::events;
use crate::forms;
use crate::page::Cx;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Toggle {
    Checkbox,
    Radio,
}

/// What an element does when activated.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Link,
    Submit,
    Reset,
    Label,
    Summary,
    Toggle(Toggle),
}

fn behavior_of(dom: &Dom, el: NodeId) -> Option<Behavior> {
    let data = dom.element(el).filter(|e| e.is_html())?;
    let type_ = data.attr("type").map(|t| t.trim().to_ascii_lowercase());
    match &*data.name.local {
        "a" | "area" => data.has_attr("href").then_some(Behavior::Link),
        "button" => match type_.as_deref() {
            Some("reset") => Some(Behavior::Reset),
            Some("button") => None,
            _ => Some(Behavior::Submit),
        },
        "input" => match type_.as_deref() {
            Some("submit") | Some("image") => Some(Behavior::Submit),
            Some("reset") => Some(Behavior::Reset),
            Some("checkbox") => Some(Behavior::Toggle(Toggle::Checkbox)),
            Some("radio") => Some(Behavior::Toggle(Toggle::Radio)),
            _ => None,
        },
        "label" => Some(Behavior::Label),
        "summary" => Some(Behavior::Summary),
        _ => None,
    }
}

/// The element whose activation behavior a click on `el` runs: the
/// nearest ancestor-or-self with one, as browsers find it along the event
/// path.
fn activation_target(dom: &Dom, el: NodeId) -> Option<(NodeId, Behavior)> {
    std::iter::once(el)
        .chain(dom.ancestors(el))
        .find_map(|n| behavior_of(dom, n).map(|b| (n, b)))
}

fn is_disabled(dom: &Dom, el: NodeId) -> bool {
    dom.element(el).is_some_and(|data| {
        data.is_html()
            && matches!(
                &*data.name.local,
                "button" | "input" | "select" | "textarea"
            )
            && data.has_attr("disabled")
    }) || forms::is_disabled(dom, el)
}

/// What a checkbox or radio button was before a click toggled it, to roll
/// the click back if a listener cancels it.
struct PreActivation {
    control: NodeId,
    was: bool,
    /// The radio button of the group that was checked before.
    previous: Option<NodeId>,
}

/// Legacy-pre-activation behavior: the new state is visible to click
/// listeners.
fn pre_activate(cx: &mut Cx<'_>, control: NodeId, kind: Toggle) -> PreActivation {
    let was = element::is_checked(cx, control);
    let previous = match kind {
        Toggle::Radio => element::checked_in_group(cx, control),
        Toggle::Checkbox => None,
    };
    element::set_checked(cx, control, kind == Toggle::Radio || !was);
    PreActivation {
        control,
        was,
        previous,
    }
}

/// After the dispatch: legacy-canceled-activation behavior when a listener
/// cancelled the click, else `input` and `change` (for a connected control
/// whose state changed).
fn post_activate(cx: &mut Cx<'_>, pre: PreActivation, proceed: bool) {
    if !proceed {
        if let Some(previous) = pre.previous {
            element::set_checked(cx, previous, true);
        }
        element::set_checked(cx, pre.control, pre.was);
        return;
    }
    if !cx.dom().is_connected(pre.control) {
        return;
    }
    if element::is_checked(cx, pre.control) != pre.was {
        events::fire(cx, EventTargetRef::Node(pre.control), "input", true, false);
        events::fire(cx, EventTargetRef::Node(pre.control), "change", true, false);
    }
}

/// Dispatches a `click` at `el` and runs the activation behavior of the
/// nearest element that has one. `trusted` distinguishes user input from
/// `element.click()`.
pub fn click(cx: &mut Cx<'_>, el: NodeId, trusted: bool) {
    click_with(cx, el, trusted, None)
}

/// `click`, with a ready-made event (the input pipeline's, carrying the
/// pointer's position).
pub(crate) fn click_with(
    cx: &mut Cx<'_>,
    el: NodeId,
    trusted: bool,
    event: Option<catpaw_js::ObjectId>,
) {
    let target = {
        let dom = cx.dom();
        if is_disabled(&dom, el) {
            return;
        }
        activation_target(&dom, el).filter(|(n, _)| !is_disabled(&dom, *n))
    };
    let pre = match target {
        Some((control, Behavior::Toggle(kind))) => Some(pre_activate(cx, control, kind)),
        _ => None,
    };
    let event = event.unwrap_or_else(|| crate::ui_events::synthetic_click(cx, trusted));
    let proceed = events::dispatch(cx, EventTargetRef::Node(el), event);
    if let Some(pre) = pre {
        post_activate(cx, pre, proceed);
        return;
    }
    if !proceed {
        return;
    }
    if let Some((control, behavior)) = target {
        run_behavior(cx, el, control, behavior, trusted);
    }
}

/// The activation steps of the DOM dispatch algorithm, for a `click`
/// `MouseEvent` script dispatches (`dispatchEvent`): the target's
/// activation behavior, or with a bubbling event the nearest ancestor's,
/// around the dispatch. Unlike `click()`, a disabled checkbox still
/// toggles. Returns what the dispatch returns.
pub(crate) fn dispatch_click(
    cx: &mut Cx<'_>,
    el: NodeId,
    event: catpaw_js::ObjectId,
    bubbles: bool,
) -> bool {
    let target = {
        let dom = cx.dom();
        behavior_of(&dom, el).map(|b| (el, b)).or_else(|| {
            if bubbles {
                dom.ancestors(el)
                    .find_map(|n| behavior_of(&dom, n).map(|b| (n, b)))
            } else {
                None
            }
        })
    };
    let pre = match target {
        Some((control, Behavior::Toggle(kind))) => Some(pre_activate(cx, control, kind)),
        _ => None,
    };
    let proceed = events::dispatch(cx, EventTargetRef::Node(el), event);
    if let Some(pre) = pre {
        post_activate(cx, pre, proceed);
        return proceed;
    }
    if proceed && let Some((control, behavior)) = target {
        let disabled = is_disabled(&cx.dom(), control);
        if !disabled {
            run_behavior(cx, el, control, behavior, false);
        }
    }
    proceed
}

/// What activating `control` does, for a click aimed at `el`.
fn run_behavior(cx: &mut Cx<'_>, el: NodeId, control: NodeId, behavior: Behavior, trusted: bool) {
    if !cx.dom().contains(control) {
        return;
    }
    match behavior {
        Behavior::Link => follow_link(cx, control),
        // The owner is looked up first: the borrow of the DOM must not
        // outlive the lookup, since submit and reset run script.
        Behavior::Submit => {
            let form = forms::form_owner(&cx.dom(), control);
            if let Some(form) = form {
                forms::submit(cx, form, Some(control), forms::Submission::Normal);
            }
        }
        Behavior::Reset => {
            let form = forms::form_owner(&cx.dom(), control);
            if let Some(form) = form {
                forms::reset(cx, form);
            }
        }
        Behavior::Label => {
            // A click on the label's own control was already its click.
            let labeled = forms::labeled_control(&cx.dom(), control);
            if let Some(labeled) = labeled
                && !cx.dom().ancestors(el).any(|a| a == labeled)
                && el != labeled
            {
                click(cx, labeled, trusted);
            }
        }
        Behavior::Summary => {
            let details = cx
                .dom()
                .parent_element(control)
                .filter(|&p| cx.dom().is_html_element(p, "details"));
            if let Some(details) = details {
                let open = cx.dom().attr(details, "open").is_some();
                if open {
                    element::remove_attr(cx, details, "open");
                } else {
                    let _ = element::set_attr(cx, details, "open", String::new());
                }
                events::fire(cx, EventTargetRef::Node(details), "toggle", false, false);
            }
        }
        Behavior::Toggle(_) => {}
    }
}

/// The browsing context a link names: its `target`, else the document's
/// `<base target>`.
fn link_target(cx: &Cx<'_>, link: NodeId) -> Option<String> {
    let dom = cx.dom();
    dom.attr(link, "target")
        .or_else(|| {
            dom.descendants(dom.document())
                .find(|&n| dom.is_html_element(n, "base") && dom.attr(n, "target").is_some())
                .and_then(|base| dom.attr(base, "target"))
        })
        .map(|t| t.trim().to_ascii_lowercase())
}

/// Follows a hyperlink: its `href` against the document base, in a new
/// window (a popup) when its target is `_blank`, else in this page (other
/// targets that would open a window open here instead).
fn follow_link(cx: &mut Cx<'_>, link: NodeId) {
    let href = cx.dom().attr(link, "href").map(str::to_string);
    let Some(href) = href else {
        return;
    };
    let base = crate::node::base_url(cx, link).unwrap_or_else(|| cx.page.base_url());
    let Ok(url) = base.join(href.trim()) else {
        return;
    };
    if url.scheme() == "javascript" {
        // `javascript:` URLs run as a script; the value they produce is
        // discarded (a page replaced with the result is rare enough).
        // Everything after `javascript:` (a `?` or `#` in the script
        // parses as a query or fragment).
        let source = percent_decode(&url[url::Position::BeforePath..]);
        if let Err(e) = cx.script.eval_script(&source, "javascript:", 1) {
            cx.report_exception(&e);
        }
        return;
    }
    // `download` saves the response instead (same-origin links only, as
    // browsers have it).
    let download = cx.dom().attr(link, "download").map(str::to_string);
    if let Some(name) = download
        && url.origin() == cx.page.url.borrow().origin()
    {
        *cx.page.navigation.borrow_mut() = Some(crate::NavigationRequest {
            download: Some(name),
            ..crate::NavigationRequest::get(url, false)
        });
        return;
    }
    if link_target(cx, link).as_deref() == Some("_blank") {
        // Opened as a popup, which needs the user's action as any does.
        let _ = crate::frames::open_popup(cx, Some(url));
        return;
    }
    crate::window::navigate(cx, url, false);
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&input[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
