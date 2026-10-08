//! Trusted input: the pointer and keyboard sequences a user's actions
//! turn into, with their default actions (focus, activation, typing,
//! implicit submission, sequential focus navigation, drag and drop).
//!
//! Positions are viewport coordinates. A click at a point hits what the
//! layout finds there; a click on an element scrolls it into view, aims at
//! the centre of its first rectangle and refuses when something else is
//! on top of it (Playwright's actionability).

use std::cell::Cell;

use catpaw_dom::{Dom, NodeId};
use catpaw_js::{EventTargetRef, ObjectId};

use crate::events::{self, Event};
use crate::generated::{self as web, InterfaceId};
use crate::page::Cx;
use crate::ui_events::{self, Input as InputData, Keyboard, Pointer, UiEvent};
use crate::{activation, dnd, element, forms, layout};

/// Why an action on an element could not be carried out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    /// The element is not in the document.
    Detached,
    /// The element has no box or an empty one.
    NotVisible,
    /// Another element covers the point aimed at.
    Occluded { by: NodeId },
    /// The element does not take text.
    NotEditable,
    /// The element is disabled.
    Disabled,
    /// Several files for a file input without `multiple`.
    TooManyFiles,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::Detached => f.write_str("the element is not in the document"),
            InputError::NotVisible => f.write_str("the element is not visible"),
            InputError::Occluded { .. } => f.write_str("another element covers it"),
            InputError::NotEditable => f.write_str("the element does not take text"),
            InputError::Disabled => f.write_str("the element is disabled"),
            InputError::TooManyFiles => f.write_str("the file input takes one file"),
        }
    }
}

impl std::error::Error for InputError {}

/// What the page remembers about the pointer and keyboard.
#[derive(Default)]
pub struct InputState {
    /// The element under the pointer, as of the last move.
    hover: Cell<Option<NodeId>>,
    pointer: Cell<(f32, f32)>,
    /// Typing changed the focused control's value since it took focus, so
    /// `change` fires when focus leaves it.
    typed: Cell<bool>,
    /// The control whose whole content is selected (Control+A): what is
    /// typed next replaces it.
    all_selected: Cell<Option<NodeId>>,
}

// ------------------------------------------------------------------ events

fn trusted(cx: &Cx<'_>, id: ObjectId) -> ObjectId {
    cx.page.note_user_activation();
    let _ = cx.page.with::<Event, _>(id, |e| e.trusted = true);
    id
}

/// The state of a mouse or pointer event of the pointer at `x`, `y`.
pub(crate) fn mouse_state(
    cx: &Cx<'_>,
    x: f32,
    y: f32,
    button: i16,
    buttons: u16,
    detail: i32,
) -> UiEvent {
    UiEvent {
        has_view: true,
        detail,
        client: (x.round() as i32, y.round() as i32),
        screen: (x.round() as i32, y.round() as i32),
        scroll: Some(layout::window_scroll(cx.page)),
        button,
        buttons,
        // The legacy `which`: the button, counted from 1, while one is
        // pressed or for the press itself.
        which: if buttons != 0 || detail > 0 {
            (button + 1) as u32
        } else {
            0
        },
        related_target: None,
        pointer: Pointer {
            pointer_id: 1,
            width: 1.0,
            height: 1.0,
            pressure: if buttons != 0 { 0.5 } else { 0.0 },
            pointer_type: "mouse".to_string(),
            is_primary: true,
            ..Pointer::default()
        },
        ..UiEvent::default()
    }
}

/// A trusted pointer or mouse event.
fn pointer_event(
    cx: &Cx<'_>,
    iface: InterfaceId,
    type_: &str,
    bubbles: bool,
    cancelable: bool,
    state: UiEvent,
) -> ObjectId {
    let id = ui_events::make(
        cx,
        iface,
        type_.to_string(),
        (bubbles, cancelable, true),
        state,
    );
    trusted(cx, id)
}

fn fire_pointer(
    cx: &mut Cx<'_>,
    target: NodeId,
    type_: &str,
    cancelable: bool,
    state: UiEvent,
) -> bool {
    let bubbles = !matches!(
        type_,
        "pointerenter" | "pointerleave" | "mouseenter" | "mouseleave"
    );
    let iface = if type_.starts_with("pointer") {
        InterfaceId::PointerEvent
    } else {
        InterfaceId::MouseEvent
    };
    let event = pointer_event(cx, iface, type_, bubbles, cancelable, state);
    events::dispatch(cx, EventTargetRef::Node(target), event)
}

/// Keys as `KeyboardEvent` reports them.
struct Key {
    key: String,
    code: String,
    key_code: u32,
    /// The character a press inserts, if any.
    text: Option<String>,
    modifiers: ui_events::Modifiers,
}

fn parse_key(spec: &str) -> Key {
    let mut modifiers = ui_events::Modifiers::default();
    let parts: Vec<&str> = spec.split('+').collect();
    let (mods, name) = match parts.split_last() {
        Some((name, mods)) if !name.is_empty() => (mods.to_vec(), *name),
        // `+` itself.
        _ => (Vec::new(), "+"),
    };
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => modifiers.ctrl = true,
            "shift" => modifiers.shift = true,
            "alt" => modifiers.alt = true,
            "meta" | "command" | "cmd" => modifiers.meta = true,
            _ => {}
        }
    }
    let single = name.chars().count() == 1;
    let (key, code, key_code, text) = if single {
        let c = name.chars().next().unwrap();
        let code = if c.is_ascii_alphabetic() {
            format!("Key{}", c.to_ascii_uppercase())
        } else if c.is_ascii_digit() {
            format!("Digit{c}")
        } else if c == ' ' {
            "Space".to_string()
        } else {
            String::new()
        };
        let key_code = if c.is_ascii_alphanumeric() {
            c.to_ascii_uppercase() as u32
        } else if c == ' ' {
            32
        } else {
            0
        };
        (name.to_string(), code, key_code, Some(name.to_string()))
    } else {
        let (key_code, text): (u32, Option<String>) = match name {
            "Enter" => (13, Some("\n".to_string())),
            "Tab" => (9, None),
            "Backspace" => (8, None),
            "Delete" => (46, None),
            "Escape" => (27, None),
            "Space" => (32, Some(" ".to_string())),
            "ArrowLeft" => (37, None),
            "ArrowUp" => (38, None),
            "ArrowRight" => (39, None),
            "ArrowDown" => (40, None),
            "Home" => (36, None),
            "End" => (35, None),
            "PageUp" => (33, None),
            "PageDown" => (34, None),
            "Shift" => (16, None),
            "Control" => (17, None),
            "Alt" => (18, None),
            "Meta" => (91, None),
            _ => (0, None),
        };
        let key = if name == "Space" {
            " ".to_string()
        } else {
            name.to_string()
        };
        let code = match name {
            "Shift" => "ShiftLeft",
            "Control" => "ControlLeft",
            "Alt" => "AltLeft",
            "Meta" => "MetaLeft",
            other => other,
        }
        .to_string();
        (key, code, key_code, text)
    };
    Key {
        key,
        code,
        key_code,
        text: if modifiers.ctrl || modifiers.meta || modifiers.alt {
            None
        } else {
            text
        },
        modifiers,
    }
}

fn key_state(key: &Key, char_code: u32) -> UiEvent {
    UiEvent {
        has_view: true,
        // The legacy `which` many pages still read: the character of a
        // keypress, the key code of the others.
        which: if char_code != 0 {
            char_code
        } else {
            key.key_code
        },
        modifiers: key.modifiers.clone(),
        key: Keyboard {
            key: key.key.clone(),
            code: key.code.clone(),
            location: 0,
            repeat: false,
            is_composing: false,
            char_code,
            // A keypress gives its character as its key code too, as
            // Chrome does.
            key_code: if char_code != 0 {
                char_code
            } else {
                key.key_code
            },
        },
        ..UiEvent::default()
    }
}

fn fire_key(cx: &mut Cx<'_>, target: NodeId, type_: &str, key: &Key) -> bool {
    let char_code = if type_ == "keypress" {
        key.text
            .as_ref()
            .and_then(|t| t.chars().next())
            .map_or(0, |c| c as u32)
    } else {
        0
    };
    let event = ui_events::make(
        cx,
        InterfaceId::KeyboardEvent,
        type_.to_string(),
        (true, type_ != "keyup", true),
        key_state(key, char_code),
    );
    let event = trusted(cx, event);
    events::dispatch(cx, EventTargetRef::Node(target), event)
}

fn fire_input(
    cx: &mut Cx<'_>,
    target: NodeId,
    type_: &str,
    input_type: &str,
    data: Option<String>,
) -> bool {
    let event = ui_events::make(
        cx,
        InterfaceId::InputEvent,
        type_.to_string(),
        (true, type_ == "beforeinput", true),
        UiEvent {
            has_view: true,
            input: InputData {
                data,
                is_composing: false,
                input_type: input_type.to_string(),
            },
            ..UiEvent::default()
        },
    );
    let event = trusted(cx, event);
    events::dispatch(cx, EventTargetRef::Node(target), event)
}

// ----------------------------------------------------------------- pointer

/// The element that receives input at a viewport point: the hit, or the
/// root element when nothing is there.
fn target_at(cx: &Cx<'_>, x: f32, y: f32) -> Option<NodeId> {
    layout::element_from_point(cx.page, x, y).or_else(|| {
        let dom = cx.dom();
        dom.child_elements(dom.document()).next()
    })
}

/// An element and the elements it is in, innermost first: what the
/// pointer is over when it is over the element.
fn hover_chain(dom: &Dom, el: NodeId) -> Vec<NodeId> {
    std::iter::once(el)
        .chain(dom.ancestors(el))
        .filter(|&n| dom.is_element(n))
        .collect()
}

/// Makes `target` the element under the pointer at `x`, `y` (with
/// `buttons` held): `pointerout` and `mouseout` at the one before and the
/// leave events at each element the pointer left, then `pointerover` and
/// `mouseover` at the new one and the enter events at each element it
/// entered, outermost first.
fn hover(cx: &mut Cx<'_>, target: NodeId, x: f32, y: f32, buttons: u16) {
    let previous = cx.page.input.hover.get().filter(|&p| cx.dom().contains(p));
    if previous == Some(target) {
        return;
    }
    cx.page.input.hover.set(Some(target));
    let (left, entered) = {
        let dom = cx.dom();
        let before = previous.map(|p| hover_chain(&dom, p)).unwrap_or_default();
        let after = hover_chain(&dom, target);
        let left: Vec<NodeId> = before
            .iter()
            .copied()
            .filter(|n| !after.contains(n))
            .collect();
        let entered: Vec<NodeId> = after
            .iter()
            .rev()
            .copied()
            .filter(|n| !before.contains(n))
            .collect();
        (left, entered)
    };
    if let Some(old) = previous {
        let mut state = mouse_state(cx, x, y, 0, buttons, 0);
        state.related_target = Some(EventTargetRef::Node(target));
        fire_pointer(cx, old, "pointerout", true, state.clone());
        for &n in &left {
            fire_pointer(cx, n, "pointerleave", false, state.clone());
        }
        fire_pointer(cx, old, "mouseout", true, state.clone());
        for &n in &left {
            fire_pointer(cx, n, "mouseleave", false, state.clone());
        }
    }
    let mut state = mouse_state(cx, x, y, 0, buttons, 0);
    state.related_target = previous.map(EventTargetRef::Node);
    fire_pointer(cx, target, "pointerover", true, state.clone());
    for &n in &entered {
        fire_pointer(cx, n, "pointerenter", false, state.clone());
    }
    fire_pointer(cx, target, "mouseover", true, state.clone());
    for &n in &entered {
        fire_pointer(cx, n, "mouseenter", false, state.clone());
    }
}

/// Moves the pointer to a point with `buttons` held: the over, out, enter
/// and leave events where the element under it changes, then
/// `pointermove` and `mousemove`.
fn move_pointer(cx: &mut Cx<'_>, x: f32, y: f32, buttons: u16) -> Option<NodeId> {
    let target = target_at(cx, x, y)?;
    cx.page.input.pointer.set((x, y));
    hover(cx, target, x, y, buttons);
    let state = mouse_state(cx, x, y, 0, buttons, 0);
    fire_pointer(cx, target, "pointermove", true, state.clone());
    fire_pointer(cx, target, "mousemove", true, state);
    Some(target)
}

/// Moves the pointer to a point: `mouseover`/`mouseout` and the enter and
/// leave events where the element under it changes, then `mousemove`.
pub fn pointer_move(cx: &mut Cx<'_>, x: f32, y: f32) -> Option<NodeId> {
    move_pointer(cx, x, y, 0)
}

/// A drag of the page's own took the pointer over: its pointer events end
/// with `pointercancel`, then `pointerout` and `pointerleave` at what it
/// was over. The next move starts over.
fn cancel_pointer(cx: &mut Cx<'_>, x: f32, y: f32) {
    let Some(old) = cx.page.input.hover.take().filter(|&p| cx.dom().contains(p)) else {
        return;
    };
    let left = hover_chain(&cx.dom(), old);
    let state = mouse_state(cx, x, y, 0, 0, 0);
    fire_pointer(cx, old, "pointercancel", false, state.clone());
    fire_pointer(cx, old, "pointerout", true, state.clone());
    for n in left {
        fire_pointer(cx, n, "pointerleave", false, state.clone());
    }
}

/// The element that takes focus when `el` is pressed: the nearest
/// focusable ancestor-or-self, if any.
fn focus_target(dom: &Dom, el: NodeId) -> Option<NodeId> {
    std::iter::once(el)
        .chain(dom.ancestors(el))
        .find(|&n| dom.is_element(n) && element::is_focusable(dom, n))
}

/// Moves focus for user input: `change` on a typed-into control first.
fn focus_for_input(cx: &mut Cx<'_>, to: Option<NodeId>) {
    let from = cx.page.document_state.borrow().focused;
    if from == to {
        return;
    }
    cx.page.input.all_selected.set(None);
    if cx.page.input.typed.replace(false)
        && let Some(old) = from.filter(|&n| cx.dom().contains(n))
    {
        events::fire(cx, EventTargetRef::Node(old), "change", true, false);
    }
    element::move_focus(cx, to);
}

/// Clicks a viewport point with the left button: the pointer and mouse
/// down, focus, up and click sequence, with activation.
/// How a click is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickOptions {
    /// As `MouseEvent.button`: 0 left, 1 middle, 2 right.
    pub button: i16,
    /// Presses in a row: 2 is a double click.
    pub count: u8,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Default for ClickOptions {
    fn default() -> Self {
        Self {
            button: 0,
            count: 1,
            ctrl: false,
            shift: false,
            alt: false,
            meta: false,
        }
    }
}

impl ClickOptions {
    /// The `buttons` bit of the button.
    fn bit(&self) -> u16 {
        match self.button {
            1 => 4,
            2 => 2,
            _ => 1,
        }
    }

    fn state(&self, cx: &Cx<'_>, x: f32, y: f32, buttons: u16, detail: i32) -> UiEvent {
        let mut state = mouse_state(cx, x, y, self.button, buttons, detail);
        state.modifiers.ctrl = self.ctrl;
        state.modifiers.shift = self.shift;
        state.modifiers.alt = self.alt;
        state.modifiers.meta = self.meta;
        state
    }
}

/// Clicks at a point as [`click_at`] does, with another button, more
/// presses or modifier keys: each press is down and up; the left button
/// clicks (and activates) each time and fires `dblclick` after a second,
/// the right one opens the context menu (`contextmenu`), the middle one
/// fires `auxclick`.
pub fn click_at_with(cx: &mut Cx<'_>, x: f32, y: f32, options: ClickOptions) -> Option<NodeId> {
    if options == ClickOptions::default() {
        return click_at(cx, x, y);
    }
    let mut target = pointer_move(cx, x, y)?;
    for press in 1..=i32::from(options.count.clamp(1, 3)) {
        let down = options.state(cx, x, y, options.bit(), press);
        let pointer_ok = fire_pointer(cx, target, "pointerdown", true, down.clone());
        let mouse_ok = !pointer_ok || fire_pointer(cx, target, "mousedown", true, down);
        if mouse_ok && press == 1 {
            let focus = focus_target(&cx.dom(), target);
            focus_for_input(cx, focus);
        }
        if options.button == 2 && press == 1 {
            let menu = options.state(cx, x, y, options.bit(), 1);
            fire_pointer(cx, target, "contextmenu", true, menu);
        }
        target = target_at(cx, x, y).filter(|&t| cx.dom().contains(t))?;
        let up = options.state(cx, x, y, 0, press);
        fire_pointer(cx, target, "pointerup", true, up.clone());
        fire_pointer(cx, target, "mouseup", true, up);
        match options.button {
            0 => {
                let click = pointer_event(
                    cx,
                    InterfaceId::PointerEvent,
                    "click",
                    true,
                    true,
                    options.state(cx, x, y, 0, press),
                );
                activation::click_with(cx, target, true, Some(click));
            }
            _ => {
                let aux = options.state(cx, x, y, 0, press);
                fire_pointer(cx, target, "auxclick", true, aux);
            }
        }
        target = target_at(cx, x, y).filter(|&t| cx.dom().contains(t))?;
        if options.button == 0 && press == 2 {
            let double = options.state(cx, x, y, 0, 2);
            fire_pointer(cx, target, "dblclick", true, double);
        }
    }
    Some(target)
}

pub fn click_at(cx: &mut Cx<'_>, x: f32, y: f32) -> Option<NodeId> {
    let target = pointer_move(cx, x, y)?;
    let down = mouse_state(cx, x, y, 0, 1, 1);
    let pointer_ok = fire_pointer(cx, target, "pointerdown", true, down.clone());
    let mouse_ok = if pointer_ok {
        fire_pointer(cx, target, "mousedown", true, down)
    } else {
        true
    };
    if mouse_ok {
        let focus = focus_target(&cx.dom(), target);
        focus_for_input(cx, focus);
    }
    // A listener may have changed the page under the pointer.
    let target = target_at(cx, x, y).filter(|&t| cx.dom().contains(t))?;
    let up = mouse_state(cx, x, y, 0, 0, 1);
    fire_pointer(cx, target, "pointerup", true, up.clone());
    fire_pointer(cx, target, "mouseup", true, up);
    let click = pointer_event(
        cx,
        InterfaceId::PointerEvent,
        "click",
        true,
        true,
        mouse_state(cx, x, y, 0, 0, 1),
    );
    activation::click_with(cx, target, true, Some(click));
    Some(target)
}

/// Clicks an element: scrolls it into view, aims at the centre of its
/// first rectangle, and clicks there if the element (or something inside
/// it) is what is there.
pub fn click_element(cx: &mut Cx<'_>, el: NodeId) -> Result<NodeId, InputError> {
    let point = aim(cx, el)?;
    Ok(click_at(cx, point.0, point.1).unwrap_or(el))
}

/// [`click_element`] with another button, more presses or modifiers.
pub fn click_element_with(
    cx: &mut Cx<'_>,
    el: NodeId,
    options: ClickOptions,
) -> Result<NodeId, InputError> {
    let point = aim(cx, el)?;
    Ok(click_at_with(cx, point.0, point.1, options).unwrap_or(el))
}

/// Where to click an element, after scrolling it into view.
fn aim(cx: &mut Cx<'_>, el: NodeId) -> Result<(f32, f32), InputError> {
    {
        let dom = cx.dom();
        if !dom.contains(el) || !dom.is_connected(el) {
            return Err(InputError::Detached);
        }
    }
    layout::scroll_into_view(
        cx,
        el,
        web::ScrollLogicalPosition::Nearest,
        web::ScrollLogicalPosition::Nearest,
    );
    aim_in_view(cx, el)
}

/// Where to click an element as the page is scrolled now: the centre of
/// its first box, which must be in the viewport and show the element there.
fn aim_in_view(cx: &Cx<'_>, el: NodeId) -> Result<(f32, f32), InputError> {
    let rect = layout::client_rects(cx.page, el)
        .into_iter()
        .find(|r| r.width > 0.0 && r.height > 0.0)
        .ok_or(InputError::NotVisible)?;
    let viewport = layout::viewport(cx.page);
    let outside = rect.x + rect.width <= 0.0
        || rect.y + rect.height <= 0.0
        || rect.x >= viewport.width
        || rect.y >= viewport.height;
    if outside {
        return Err(InputError::NotVisible);
    }
    let x = (rect.x + rect.width / 2.0).clamp(0.0, viewport.width - 1.0);
    let y = (rect.y + rect.height / 2.0).clamp(0.0, viewport.height - 1.0);
    let hit = layout::element_from_point(cx.page, x, y);
    match hit {
        Some(hit) if shows(&cx.dom(), hit, el) => Ok((x, y)),
        Some(by) => Err(InputError::Occluded { by }),
        None => Err(InputError::NotVisible),
    }
}

/// Whether the element `hit` at a point counts as `el` being there: it is
/// `el`, inside it, or around it (a label and its control count as one).
fn shows(dom: &Dom, hit: NodeId, el: NodeId) -> bool {
    hit == el || dom.ancestors(hit).any(|a| a == el) || dom.ancestors(el).any(|a| a == hit)
}

/// Moves of the pointer between the press and the release of a drag: some
/// libraries wait for several.
const DRAG_STEPS: u32 = 5;

/// Drags `el` onto `onto` with the left button: down at the centre of the
/// one, across in steps, up at the centre of the other, which must be what
/// the pointer is over then (or the dragged element itself, which pages
/// often move along with the pointer). Both must show in the viewport at
/// once.
///
/// A draggable element (`draggable=true`, a link, an image) goes as HTML
/// drag and drop has it, from `dragstart` to `dragend` with a
/// `DataTransfer`, the pointer events cancelled and no mouse events on the
/// way; when the drop would miss `onto`, the drag is cancelled instead.
/// Anything else (or a drag a `mousedown` or `dragstart` listener
/// cancelled) goes as mouse events only.
pub fn drag_element(cx: &mut Cx<'_>, el: NodeId, onto: NodeId) -> Result<(), InputError> {
    // Aiming at the drop target may scroll the dragged element away: both
    // must still show at the scroll that leaves.
    aim(cx, el)?;
    aim(cx, onto)?;
    let from = aim_in_view(cx, el)?;
    let to = aim_in_view(cx, onto)?;
    let target = pointer_move(cx, from.0, from.1).ok_or(InputError::NotVisible)?;
    // The press must land on what is dragged.
    if !shows(&cx.dom(), target, el) {
        return Err(InputError::Occluded { by: target });
    }
    let down = mouse_state(cx, from.0, from.1, 0, 1, 1);
    // A cancelled `pointerdown` leaves out `mousedown`; a cancelled
    // `mousedown` keeps the element from being dragged.
    let pressed = !fire_pointer(cx, target, "pointerdown", true, down.clone())
        || fire_pointer(cx, target, "mousedown", true, down);
    let point = |step: u32| {
        let t = step as f32 / DRAG_STEPS as f32;
        (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
    };
    // Released over `end`, the drop reached `onto`.
    let landed = |dom: &Dom, end: NodeId| shows(dom, end, onto) || shows(dom, end, el);
    let source = if pressed {
        dnd::drag_source(&cx.dom(), target)
    } else {
        None
    };
    if let Some(mut drag) = source.and_then(|source| dnd::Drag::start(cx, source, from)) {
        cancel_pointer(cx, from.0, from.1);
        for step in 1..=DRAG_STEPS {
            let (x, y) = point(step);
            cx.page.input.pointer.set((x, y));
            let under = target_at(cx, x, y);
            if !drag.step(cx, (x, y), under) {
                // The page cancelled the drag: it ends there.
                drag.finish(cx, false);
                return Ok(());
            }
        }
        let end = drag.current();
        let lands = end.is_some_and(|end| landed(&cx.dom(), end));
        drag.finish(cx, lands);
        return match end {
            Some(_) if lands => Ok(()),
            Some(by) => Err(InputError::Occluded { by }),
            None => Err(InputError::NotVisible),
        };
    }
    for step in 1..=DRAG_STEPS {
        let (x, y) = point(step);
        move_pointer(cx, x, y, 1);
    }
    let end = target_at(cx, to.0, to.1).ok_or(InputError::NotVisible)?;
    let up = mouse_state(cx, to.0, to.1, 0, 0, 1);
    fire_pointer(cx, end, "pointerup", true, up.clone());
    fire_pointer(cx, end, "mouseup", true, up);
    if landed(&cx.dom(), end) {
        Ok(())
    } else {
        Err(InputError::Occluded { by: end })
    }
}

/// Moves the pointer over an element.
pub fn hover_element(cx: &mut Cx<'_>, el: NodeId) -> Result<(), InputError> {
    let (x, y) = aim(cx, el)?;
    pointer_move(cx, x, y);
    Ok(())
}

// ---------------------------------------------------------------- keyboard

/// Whether an element takes typed text, and how.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Editable {
    /// `input` of a text-like type.
    TextInput,
    TextArea,
    Content,
}

fn editable(dom: &Dom, el: NodeId) -> Option<Editable> {
    let data = dom.element(el)?;
    if data.is_html() {
        match &*data.name.local {
            "input" => {
                let type_ = data.attr("type").map(|t| t.trim().to_ascii_lowercase());
                let text_like = matches!(
                    type_.as_deref(),
                    None | Some(
                        "text"
                            | "search"
                            | "url"
                            | "tel"
                            | "email"
                            | "password"
                            | "number"
                            | "date"
                            | "month"
                            | "week"
                            | "time"
                            | "datetime-local"
                    )
                );
                return (text_like && !data.has_attr("readonly") && !data.has_attr("disabled"))
                    .then_some(Editable::TextInput);
            }
            "textarea" => {
                return (!data.has_attr("readonly") && !data.has_attr("disabled"))
                    .then_some(Editable::TextArea);
            }
            _ => {}
        }
    }
    let editable = std::iter::once(el).chain(dom.ancestors(el)).find_map(|n| {
        dom.attr(n, "contenteditable")
            .map(|v| !v.eq_ignore_ascii_case("false"))
    });
    editable.unwrap_or(false).then_some(Editable::Content)
}

fn control_value(cx: &mut Cx<'_>, el: NodeId, kind: Editable) -> String {
    match kind {
        Editable::TextInput => {
            <crate::Web as web::HTMLInputElementImpl>::value(cx, el).unwrap_or_default()
        }
        Editable::TextArea => {
            <crate::Web as web::HTMLTextAreaElementImpl>::value(cx, el).unwrap_or_default()
        }
        Editable::Content => cx.dom().text_content(el),
    }
}

/// Sets the value as the user's edit (the dirty value, kept apart from
/// the `value` attribute).
fn set_control_value(cx: &mut Cx<'_>, el: NodeId, kind: Editable, value: String) {
    match kind {
        Editable::TextInput | Editable::TextArea => {
            cx.page.form_state.borrow_mut().entry(el).or_default().value = Some(value);
        }
        Editable::Content => {
            let _ = <crate::Web as web::NodeImpl>::set_text_content(cx, el, Some(value));
        }
    }
}

/// The element keyboard input goes to: the focused element, else the body.
fn keyboard_target(cx: &Cx<'_>) -> Option<NodeId> {
    let dom = cx.dom();
    cx.page
        .document_state
        .borrow()
        .focused
        .filter(|&n| dom.contains(n) && dom.is_connected(n))
        .or_else(|| {
            dom.descendants(dom.document())
                .find(|&n| dom.is_html_element(n, "body"))
        })
}

/// Edits the value of an editable element through `beforeinput` and
/// `input`; `false` when `beforeinput` was cancelled.
fn edit_value(
    cx: &mut Cx<'_>,
    el: NodeId,
    kind: Editable,
    input_type: &str,
    data: Option<String>,
    edit: impl FnOnce(String) -> String,
) -> bool {
    if !fire_input(cx, el, "beforeinput", input_type, data.clone()) {
        return false;
    }
    // A selection of everything is replaced by the edit.
    let current = if cx.page.input.all_selected.get() == Some(el) {
        cx.page.input.all_selected.set(None);
        String::new()
    } else {
        control_value(cx, el, kind)
    };
    let next = edit(current);
    set_control_value(cx, el, kind, next);
    cx.page.input.typed.set(true);
    fire_input(cx, el, "input", input_type, data);
    true
}

/// Types text into the focused element, one character at a time, as key
/// presses that insert.
pub fn type_text(cx: &mut Cx<'_>, text: &str) -> Result<(), InputError> {
    let Some(target) = keyboard_target(cx) else {
        return Err(InputError::Detached);
    };
    let kind = editable(&cx.dom(), target);
    for ch in text.chars() {
        let spec = match ch {
            '\n' => "Enter".to_string(),
            '\t' => "Tab".to_string(),
            c => c.to_string(),
        };
        press_on(cx, target, &parse_key(&spec), kind);
        if !cx.dom().contains(target) {
            break;
        }
    }
    Ok(())
}

/// Presses a key (`Enter`, `Tab`, `a`, `Shift+Tab`, `Control+a`) on the
/// focused element, with its default action.
pub fn press(cx: &mut Cx<'_>, spec: &str) -> Result<(), InputError> {
    let Some(target) = keyboard_target(cx) else {
        return Err(InputError::Detached);
    };
    let kind = editable(&cx.dom(), target);
    press_on(cx, target, &parse_key(spec), kind);
    Ok(())
}

fn press_on(cx: &mut Cx<'_>, target: NodeId, key: &Key, kind: Option<Editable>) {
    let proceed = fire_key(cx, target, "keydown", key);
    let mut default_done = false;
    if proceed {
        if let Some(text) = &key.text {
            let keypress_ok = fire_key(cx, target, "keypress", key);
            if keypress_ok {
                default_done = true;
                match (key.key.as_str(), kind) {
                    ("Enter", Some(Editable::TextInput)) => {
                        forms::implicit_submission(cx, target);
                    }
                    ("Enter", Some(Editable::TextArea)) | ("Enter", Some(Editable::Content)) => {
                        edit_value(cx, target, kind.unwrap(), "insertLineBreak", None, |v| {
                            v + "\n"
                        });
                    }
                    ("Enter", None) | (" ", None) => {
                        // Buttons, links and checkboxes activate from the keyboard.
                        let activates = cx.dom().element(target).is_some_and(|el| {
                            el.is_html()
                                && (matches!(&*el.name.local, "button" | "a" | "summary")
                                    || (&*el.name.local == "input"
                                        && el.attr("type").is_some_and(|t| {
                                            matches!(
                                                t.to_ascii_lowercase().as_str(),
                                                "button"
                                                    | "submit"
                                                    | "reset"
                                                    | "checkbox"
                                                    | "radio"
                                                    | "image"
                                            )
                                        })))
                        });
                        if activates {
                            activation::click(cx, target, true);
                        }
                    }
                    (_, Some(editable)) => {
                        let text = text.clone();
                        edit_value(
                            cx,
                            target,
                            editable,
                            "insertText",
                            Some(text.clone()),
                            |v| v + &text,
                        );
                    }
                    _ => default_done = false,
                }
            }
        } else {
            match key.key.as_str() {
                "Backspace" | "Delete" => {
                    if let Some(editable) = kind {
                        default_done = true;
                        edit_value(
                            cx,
                            target,
                            editable,
                            "deleteContentBackward",
                            None,
                            |mut v| {
                                v.pop();
                                v
                            },
                        );
                    }
                }
                "Tab" => {
                    default_done = true;
                    let next = next_focusable(cx, target, key.modifiers.shift);
                    focus_for_input(cx, next);
                }
                "a" | "A"
                    if (key.modifiers.ctrl || key.modifiers.meta)
                        && !key.modifiers.alt
                        && kind.is_some() =>
                {
                    default_done = true;
                    cx.page.input.all_selected.set(Some(target));
                }
                "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End"
                    if !key.modifiers.shift =>
                {
                    cx.page.input.all_selected.set(None);
                }
                _ => {}
            }
        }
    }
    let _ = default_done;
    if cx.dom().contains(target) {
        fire_key(cx, target, "keyup", key);
    }
}

/// The element after (or before) `from` in sequential focus order.
fn next_focusable(cx: &mut Cx<'_>, from: NodeId, backwards: bool) -> Option<NodeId> {
    let candidates: Vec<NodeId> = {
        let dom = cx.dom();
        dom.descendants(dom.document())
            .filter(|&n| {
                dom.is_element(n)
                    && element::is_focusable(&dom, n)
                    && !dom
                        .attr(n, "tabindex")
                        .and_then(|t| t.trim().parse::<i32>().ok())
                        .is_some_and(|t| t < 0)
            })
            .collect()
    };
    let visible: Vec<NodeId> = candidates
        .into_iter()
        .filter(|&n| layout::bounding_client_rect(cx.page, n).width > 0.0)
        .collect();
    let position = visible.iter().position(|&n| n == from);
    match (position, backwards) {
        (Some(i), false) => visible.get(i + 1).copied(),
        (Some(0), true) | (None, true) => visible.last().copied(),
        (Some(i), true) => visible.get(i - 1).copied(),
        (None, false) => {
            // From an unfocusable element: the next focusable one after
            // it in tree order.
            let dom = cx.dom();
            let mut after = false;
            let mut found = None;
            for n in dom.descendants(dom.document()) {
                if n == from {
                    after = true;
                } else if after && visible.contains(&n) {
                    found = Some(n);
                    break;
                }
            }
            found.or_else(|| visible.first().copied())
        }
    }
}

// ----------------------------------------------------------- compositions

/// Focuses an element as a user would (clicking it without the click).
pub fn focus(cx: &mut Cx<'_>, el: NodeId) -> Result<(), InputError> {
    if !cx.dom().contains(el) {
        return Err(InputError::Detached);
    }
    let target = focus_target(&cx.dom(), el).ok_or(InputError::NotEditable)?;
    focus_for_input(cx, Some(target));
    Ok(())
}

/// Replaces an editable element's value with `text` in one go, as a
/// paste would: focus, one `input`, then `change`.
pub fn fill(cx: &mut Cx<'_>, el: NodeId, text: &str) -> Result<(), InputError> {
    {
        let dom = cx.dom();
        if !dom.contains(el) || !dom.is_connected(el) {
            return Err(InputError::Detached);
        }
        if forms::is_disabled(&dom, el) {
            return Err(InputError::Disabled);
        }
    }
    let kind = editable(&cx.dom(), el).ok_or(InputError::NotEditable)?;
    // Not in one statement: the borrow of the document would last through
    // the focus listeners, which may change it.
    let focus = focus_target(&cx.dom(), el).or(Some(el));
    focus_for_input(cx, focus);
    let text = text.to_string();
    if control_value(cx, el, kind) != text {
        edit_value(
            cx,
            el,
            kind,
            "insertReplacementText",
            Some(text.clone()),
            |_| text,
        );
        cx.page.input.typed.set(false);
        events::fire(cx, EventTargetRef::Node(el), "change", true, false);
    }
    Ok(())
}

/// Checks or unchecks a checkbox or radio button by clicking it when its
/// state differs.
pub fn set_checked(cx: &mut Cx<'_>, el: NodeId, checked: bool) -> Result<(), InputError> {
    if element::is_checked(cx, el) == checked {
        return Ok(());
    }
    click_element(cx, el).map(drop)
}

/// Selects the option of a `select` whose value (or text) is `value`,
/// with `input` and `change`.
pub fn select_option(cx: &mut Cx<'_>, select: NodeId, value: &str) -> Result<(), InputError> {
    let option = {
        let dom = cx.dom();
        let options = forms::options_of(&dom, select);
        options
            .iter()
            .copied()
            .find(|&o| forms::option_value(&dom, o) == value)
            .or_else(|| {
                options
                    .iter()
                    .copied()
                    .find(|&o| forms::option_text(&dom, o) == value)
            })
    };
    match option {
        Some(option) => select_options(cx, select, &[option]),
        None if !cx.dom().contains(select) => Err(InputError::Detached),
        None => Err(InputError::NotEditable),
    }
}

/// Chooses files in an `<input type=file>`, as its file picker would:
/// each file is a name, a MIME type and the bytes; more than one only
/// when the input has `multiple`. The input fires `input` and `change`.
pub fn choose_files(
    cx: &mut Cx<'_>,
    input: NodeId,
    files: Vec<(String, String, Vec<u8>)>,
) -> Result<(), InputError> {
    {
        let dom = cx.dom();
        if !dom.contains(input) || !dom.is_connected(input) {
            return Err(InputError::Detached);
        }
        let is_file = dom.is_html_element(input, "input")
            && dom
                .attr(input, "type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("file"));
        if !is_file {
            return Err(InputError::NotEditable);
        }
        if forms::is_disabled(&dom, input) {
            return Err(InputError::Disabled);
        }
        if files.len() > 1 && dom.attr(input, "multiple").is_none() {
            return Err(InputError::TooManyFiles);
        }
    }
    focus_for_input(cx, Some(input));
    crate::file_api::choose_files(cx, input, files);
    events::fire(cx, EventTargetRef::Node(input), "input", true, false);
    events::fire(cx, EventTargetRef::Node(input), "change", true, false);
    Ok(())
}

/// Selects exactly `options` of a `select` (only the first, in a
/// single-select), with `input` and `change`, as a user picking them would.
pub fn select_options(
    cx: &mut Cx<'_>,
    select: NodeId,
    options: &[NodeId],
) -> Result<(), InputError> {
    let (multiple, all) = {
        let dom = cx.dom();
        if !dom.contains(select) || !dom.is_connected(select) {
            return Err(InputError::Detached);
        }
        if forms::is_disabled(&dom, select) {
            return Err(InputError::Disabled);
        }
        (
            dom.attr(select, "multiple").is_some(),
            forms::options_of(&dom, select),
        )
    };
    let Some(&first) = options.first() else {
        return Err(InputError::NotEditable);
    };
    if !all.contains(&first) {
        return Err(InputError::NotEditable);
    }
    focus_for_input(cx, Some(select));
    if multiple {
        for option in all {
            forms::select_option(cx, option, options.contains(&option));
        }
    } else {
        forms::select_option(cx, first, true);
    }
    events::fire(cx, EventTargetRef::Node(select), "input", true, false);
    events::fire(cx, EventTargetRef::Node(select), "change", true, false);
    Ok(())
}
