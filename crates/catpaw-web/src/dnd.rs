//! HTML drag and drop (<https://html.spec.whatwg.org/multipage/dnd.html>):
//! the data a drag carries, seen through `DataTransfer` objects, and the
//! events a drag of an element fires from `dragstart` to `dragend`.
//!
//! Only elements are dragged, never selections or files: a drag carries
//! what its `dragstart` listeners put in it (a link or an image starts
//! with its URL), and nothing is drawn while it goes on. The drag is run
//! by the agent's drags (`input::drag_element`), in steps.

use std::cell::RefCell;
use std::rc::Rc;

use catpaw_dom::{Dom, NodeId};
use catpaw_js::{EventTargetRef, Fallible, ObjectId};

use crate::events::{self, Event};
use crate::generated::{self as web, InterfaceId};
use crate::page::Cx;
use crate::{Web, file_api, input, platform_object, ui_events};

/// What a `DataTransfer` may do with the data of a drag, which depends on
/// the event it came with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// During `dragstart`, and for one script made: anything.
    ReadWrite,
    /// During `drop`: the data can be read.
    ReadOnly,
    /// Otherwise: the types show, the data does not.
    Protected,
}

/// The drag data store: text items by type, in the order they came.
struct Store {
    items: Vec<(String, String)>,
    mode: Mode,
    /// The effects the drag allows, as `dragstart` left `effectAllowed`.
    allowed: String,
}

impl Store {
    fn shared(items: Vec<(String, String)>, mode: Mode, allowed: &str) -> Rc<RefCell<Store>> {
        Rc::new(RefCell::new(Store {
            items,
            mode,
            allowed: allowed.to_string(),
        }))
    }
}

/// A `DataTransfer`: a view of a drag's store, made for one event (or by
/// script, with a store of its own).
pub struct DataTransferObject {
    store: Rc<RefCell<Store>>,
    drop_effect: String,
    effect_allowed: String,
}
platform_object!(DataTransferObject, DataTransfer);

fn transfer<R>(
    cx: &Cx<'_>,
    id: ObjectId,
    f: impl FnOnce(&mut DataTransferObject) -> R,
) -> Fallible<R> {
    cx.page.with::<DataTransferObject, _>(id, f)
}

/// A format as the store keys it: lowercase, `text` and `url` standing for
/// `text/plain` and `text/uri-list`.
fn normalize(format: &str) -> String {
    match format.to_ascii_lowercase().as_str() {
        "text" => "text/plain".to_string(),
        "url" => "text/uri-list".to_string(),
        other => other.to_string(),
    }
}

/// The first URL of `text/uri-list` data (lines starting with `#` are
/// comments).
fn first_url(list: &str) -> String {
    list.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or_default()
        .to_string()
}

/// The values `effectAllowed` takes.
const EFFECTS_ALLOWED: &[&str] = &[
    "none",
    "copy",
    "copyLink",
    "copyMove",
    "link",
    "linkMove",
    "move",
    "all",
    "uninitialized",
];

impl web::DataTransferImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(DataTransferObject {
            store: Store::shared(Vec::new(), Mode::ReadWrite, "none"),
            drop_effect: "none".to_string(),
            effect_allowed: "none".to_string(),
        }))
    }

    fn drop_effect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        transfer(cx, this, |t| t.drop_effect.clone())
    }

    fn set_drop_effect(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        transfer(cx, this, |t| {
            if matches!(value.as_str(), "none" | "copy" | "link" | "move") {
                t.drop_effect = value;
            }
        })
    }

    fn effect_allowed(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        transfer(cx, this, |t| t.effect_allowed.clone())
    }

    fn set_effect_allowed(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        transfer(cx, this, |t| {
            if t.store.borrow().mode == Mode::ReadWrite && EFFECTS_ALLOWED.contains(&value.as_str())
            {
                t.effect_allowed = value;
            }
        })
    }

    fn types(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<String>> {
        transfer(cx, this, |t| {
            t.store
                .borrow()
                .items
                .iter()
                .map(|(type_, _)| type_.clone())
                .collect()
        })
    }

    fn get_data(cx: &mut Cx<'_>, this: ObjectId, format: String) -> Fallible<String> {
        transfer(cx, this, |t| {
            let store = t.store.borrow();
            if store.mode == Mode::Protected {
                return String::new();
            }
            let wanted = normalize(&format);
            let data = store
                .items
                .iter()
                .find(|(type_, _)| *type_ == wanted)
                .map(|(_, data)| data.as_str())
                .unwrap_or_default();
            if format.eq_ignore_ascii_case("url") {
                first_url(data)
            } else {
                data.to_string()
            }
        })
    }

    fn set_data(cx: &mut Cx<'_>, this: ObjectId, format: String, data: String) -> Fallible<()> {
        transfer(cx, this, |t| {
            let mut store = t.store.borrow_mut();
            if store.mode != Mode::ReadWrite {
                return;
            }
            let format = normalize(&format);
            store.items.retain(|(type_, _)| *type_ != format);
            store.items.push((format, data));
        })
    }

    fn clear_data(cx: &mut Cx<'_>, this: ObjectId, format: Option<String>) -> Fallible<()> {
        transfer(cx, this, |t| {
            let mut store = t.store.borrow_mut();
            if store.mode != Mode::ReadWrite {
                return;
            }
            match format {
                Some(format) => {
                    let format = normalize(&format);
                    store.items.retain(|(type_, _)| *type_ != format);
                }
                None => store.items.clear(),
            }
        })
    }

    fn files(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        transfer(cx, this, |_| ())?;
        Ok(file_api::empty_file_list(cx))
    }
}

// ------------------------------------------------------------- the drag

/// Whether an element is draggable (the `draggable` IDL attribute): as its
/// `draggable` attribute says, else when it is an image or a link.
fn is_draggable(dom: &Dom, el: NodeId) -> bool {
    let Some(data) = dom.element(el).filter(|data| data.is_html()) else {
        return false;
    };
    match data.attr("draggable") {
        Some(value) if value.eq_ignore_ascii_case("true") => true,
        Some(value) if value.eq_ignore_ascii_case("false") => false,
        _ => match &*data.name.local {
            "img" => true,
            "a" => data.has_attr("href"),
            _ => false,
        },
    }
}

/// What a press at `node` drags: the nearest draggable element at or
/// above it, if any.
pub(crate) fn drag_source(dom: &Dom, node: NodeId) -> Option<NodeId> {
    std::iter::once(node)
        .chain(dom.ancestors(node))
        .find(|&n| is_draggable(dom, n))
}

/// A drag of an element under way: the drag-and-drop processing model's
/// state between the steps of the pointer.
pub(crate) struct Drag {
    source: NodeId,
    store: Rc<RefCell<Store>>,
    /// Whether a link is dragged: it is linked to, by default.
    link: bool,
    /// The element under the pointer, as the last step found it (the
    /// current target element).
    current: Option<NodeId>,
    /// What the current target said it would do with a drop (the current
    /// drag operation): `none`, `copy`, `link` or `move`.
    operation: String,
    at: (f32, f32),
}

/// <https://html.spec.whatwg.org/multipage/dnd.html#dndevents>: the
/// operation a target that cancelled `dragover` takes: the `dropEffect`
/// it left, when the effects the drag allows (`copyLink`, `all`, ...)
/// take it in.
fn operation(allowed: &str, drop_effect: &str) -> &'static str {
    let allows = |effect: &str| {
        matches!(allowed, "uninitialized" | "all") || allowed.to_ascii_lowercase().contains(effect)
    };
    match drop_effect {
        "copy" if allows("copy") => "copy",
        "link" if allows("link") => "link",
        "move" if allows("move") => "move",
        _ => "none",
    }
}

impl Drag {
    /// Starts dragging `source`, pressed at `at`: `dragstart`, whose
    /// listeners fill the store. `None` when one cancelled it: there is
    /// no drag then.
    pub(crate) fn start(cx: &mut Cx<'_>, source: NodeId, at: (f32, f32)) -> Option<Drag> {
        let (url, link) = {
            let dom = cx.dom();
            let url = match dom.element(source).map(|data| &*data.name.local) {
                Some("a") => dom.attr(source, "href"),
                Some("img") => dom.attr(source, "src"),
                _ => None,
            }
            .map(str::to_string);
            (url, dom.is_html_element(source, "a"))
        };
        let items = url
            .and_then(|url| cx.page.resolve_url(&url))
            .map(|url| vec![("text/uri-list".to_string(), url.to_string())])
            .unwrap_or_default();
        let drag = Drag {
            source,
            store: Store::shared(items, Mode::Protected, "uninitialized"),
            link,
            current: None,
            operation: "none".to_string(),
            at,
        };
        let (started, _, allowed) = drag.fire(cx, "dragstart", source, None);
        if !started {
            return None;
        }
        drag.store.borrow_mut().allowed = allowed;
        Some(drag)
    }

    /// The element under the pointer, as the last step found it.
    pub(crate) fn current(&self) -> Option<NodeId> {
        self.current
    }

    /// Moves the pointer to `at`, over `under`: `drag` at the source, then
    /// `dragenter` and `dragleave` when the element under the pointer
    /// changes, and `dragover` at it. `false` when a `drag` listener
    /// cancelled the drag.
    pub(crate) fn step(&mut self, cx: &mut Cx<'_>, at: (f32, f32), under: Option<NodeId>) -> bool {
        self.at = at;
        let (going, _, _) = self.fire(cx, "drag", self.source, None);
        if !going {
            self.operation = "none".to_string();
            return false;
        }
        if under != self.current {
            let previous = self.current;
            if let Some(under) = under {
                self.fire(cx, "dragenter", under, previous);
            }
            self.current = under;
            if let Some(previous) = previous {
                self.fire(cx, "dragleave", previous, under);
            }
        }
        self.operation = match self.current {
            Some(current) => {
                let (ignored, drop_effect, _) = self.fire(cx, "dragover", current, None);
                if ignored {
                    "none".to_string()
                } else {
                    let allowed = self.store.borrow().allowed.clone();
                    operation(&allowed, &drop_effect).to_string()
                }
            }
            None => "none".to_string(),
        };
        true
    }

    /// Ends the drag where the pointer is: `drop` there when `drop` is
    /// asked for and the last `dragover` there took it, else `dragleave`
    /// (the drop fails, as when the user cancels a drag); then `dragend`.
    pub(crate) fn finish(mut self, cx: &mut Cx<'_>, drop: bool) {
        match self.current {
            Some(target) if drop && self.operation != "none" => {
                let (ignored, drop_effect, _) = self.fire(cx, "drop", target, None);
                // A drop no listener took has no effect here (no text
                // control takes the data).
                self.operation = if ignored {
                    "none".to_string()
                } else {
                    drop_effect
                };
            }
            Some(target) => {
                self.operation = "none".to_string();
                self.fire(cx, "dragleave", target, None);
            }
            None => self.operation = "none".to_string(),
        }
        self.fire(cx, "dragend", self.source, None);
    }

    /// The `dropEffect` a `dragenter` or `dragover` starts with, by the
    /// effects the drag allows.
    fn default_drop_effect(&self) -> &'static str {
        match self.store.borrow().allowed.as_str() {
            "none" => "none",
            "link" | "linkMove" => "link",
            "move" => "move",
            "uninitialized" if self.link => "link",
            _ => "copy",
        }
    }

    /// Fires the drag event `type_` at `target`, with a `DataTransfer` of
    /// the store made for it. Returns whether no listener cancelled it,
    /// and the `dropEffect` and `effectAllowed` it was left with.
    fn fire(
        &self,
        cx: &mut Cx<'_>,
        type_: &str,
        target: NodeId,
        related: Option<NodeId>,
    ) -> (bool, String, String) {
        let (mode, drop_effect) = match type_ {
            "dragstart" => (Mode::ReadWrite, "none"),
            "dragenter" | "dragover" => (Mode::Protected, self.default_drop_effect()),
            "drop" => (Mode::ReadOnly, self.operation.as_str()),
            "dragend" => (Mode::Protected, self.operation.as_str()),
            _ => (Mode::Protected, "none"),
        };
        let allowed = {
            let mut store = self.store.borrow_mut();
            store.mode = mode;
            store.allowed.clone()
        };
        let transfer = cx.page.alloc(DataTransferObject {
            store: self.store.clone(),
            drop_effect: drop_effect.to_string(),
            effect_allowed: allowed,
        });
        // The event holds it as long as the event lives, and the drag
        // until the effects it was left with are read.
        cx.pin(transfer);
        cx.pin(transfer);
        // The button is held until the drop.
        let buttons = if matches!(type_, "drop" | "dragend") {
            0
        } else {
            1
        };
        let mut state = input::mouse_state(cx, self.at.0, self.at.1, 0, buttons, 0);
        state.related_target = related.map(EventTargetRef::Node);
        state.data_transfer = Some(transfer);
        let cancelable = !matches!(type_, "dragleave" | "dragend");
        let event = ui_events::make(
            cx,
            InterfaceId::DragEvent,
            type_.to_string(),
            (true, cancelable, true),
            state,
        );
        let _ = cx.page.with::<Event, _>(event, |e| e.trusted = true);
        let proceed = events::dispatch(cx, EventTargetRef::Node(target), event);
        self.store.borrow_mut().mode = Mode::Protected;
        let (drop_effect, effect_allowed) = cx
            .page
            .try_with::<DataTransferObject, _>(transfer, |t| {
                (t.drop_effect.clone(), t.effect_allowed.clone())
            })
            .unwrap_or_default();
        cx.unpin(transfer);
        (proceed, drop_effect, effect_allowed)
    }
}
