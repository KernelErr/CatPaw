//! The UI event family: `UIEvent`, `FocusEvent`, `InputEvent`,
//! `KeyboardEvent`, `MouseEvent`, `WheelEvent` and `PointerEvent`.
//!
//! One state record serves them all; each interface reads its own part.
//! Without layout, coordinates are what the constructor was given, and the
//! page-relative and offset coordinates equal the client ones (the page is
//! not scrolled and no box has a position).

use catpaw_js::{EventTargetRef, Fallible, ObjectId, WindowRef};

use crate::Web;
use crate::events::{Event, EventData};
use crate::generated::{self as web, InterfaceId};
use crate::page::Cx;

/// The state of a UI event, whichever interface it is.
#[derive(Clone, Debug, Default)]
pub struct UiEvent {
    pub has_view: bool,
    pub detail: i32,
    pub which: u32,
    pub modifiers: Modifiers,
    pub screen: (i32, i32),
    pub client: (i32, i32),
    pub button: i16,
    pub buttons: u16,
    pub related_target: Option<EventTargetRef>,
    pub key: Keyboard,
    pub input: Input,
    pub wheel: Wheel,
    pub pointer: Pointer,
}

#[derive(Clone, Debug, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
    pub alt_graph: bool,
    pub caps_lock: bool,
    pub fn_: bool,
    pub fn_lock: bool,
    pub hyper: bool,
    pub num_lock: bool,
    pub scroll_lock: bool,
    pub super_: bool,
    pub symbol: bool,
    pub symbol_lock: bool,
}

impl Modifiers {
    /// <https://w3c.github.io/uievents-key/#keys-modifier>
    fn state(&self, key: &str) -> bool {
        match key {
            "Control" => self.ctrl,
            "Shift" => self.shift,
            "Alt" => self.alt,
            "Meta" => self.meta,
            "AltGraph" => self.alt_graph,
            "CapsLock" => self.caps_lock,
            "Fn" => self.fn_,
            "FnLock" => self.fn_lock,
            "Hyper" => self.hyper,
            "NumLock" => self.num_lock,
            "ScrollLock" => self.scroll_lock,
            "Super" => self.super_,
            "Symbol" => self.symbol,
            "SymbolLock" => self.symbol_lock,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Keyboard {
    pub key: String,
    pub code: String,
    pub location: u32,
    pub repeat: bool,
    pub is_composing: bool,
    pub char_code: u32,
    pub key_code: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Input {
    pub data: Option<String>,
    pub is_composing: bool,
    pub input_type: String,
}

#[derive(Clone, Debug, Default)]
pub struct Wheel {
    pub delta: (f64, f64, f64),
    pub delta_mode: u32,
}

#[derive(Clone, Debug)]
pub struct Pointer {
    pub pointer_id: i32,
    pub width: f64,
    pub height: f64,
    pub pressure: f64,
    pub tangential_pressure: f64,
    pub tilt: (i32, i32),
    pub twist: i32,
    pub pointer_type: String,
    pub is_primary: bool,
}

impl Default for Pointer {
    fn default() -> Self {
        Self {
            pointer_id: 0,
            width: 1.0,
            height: 1.0,
            pressure: 0.0,
            tangential_pressure: 0.0,
            tilt: (0, 0),
            twist: 0,
            pointer_type: String::new(),
            is_primary: false,
        }
    }
}

/// Makes a UI event of interface `iface`.
pub(crate) fn make(
    cx: &Cx<'_>,
    iface: InterfaceId,
    type_: String,
    flags: (bool, bool, bool),
    state: UiEvent,
) -> ObjectId {
    let (bubbles, cancelable, composed) = flags;
    let mut event = Event::new(type_, bubbles, cancelable, cx.page.clock.peek());
    event.iface = iface;
    event.composed = composed;
    event.data = EventData::Ui(Box::new(state));
    cx.page.alloc(event)
}

/// A synthetic `click`, as `element.click()` and the agent's own clicks
/// dispatch it: a pointer event with a mouse's buttons.
pub(crate) fn synthetic_click(cx: &Cx<'_>, trusted: bool) -> ObjectId {
    let state = UiEvent {
        has_view: true,
        buttons: 0,
        pointer: Pointer {
            is_primary: true,
            pointer_type: if trusted {
                "mouse".to_string()
            } else {
                String::new()
            },
            ..Pointer::default()
        },
        ..UiEvent::default()
    };
    let id = make(
        cx,
        InterfaceId::PointerEvent,
        "click".to_string(),
        (true, true, true),
        state,
    );
    let _ = cx.page.with::<Event, _>(id, |e| e.trusted = trusted);
    id
}

fn ui<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&UiEvent) -> R) -> Fallible<R> {
    let fallback = UiEvent::default();
    cx.page.with::<Event, _>(this, |e| match &e.data {
        EventData::Ui(state) => f(state),
        _ => f(&fallback),
    })
}

fn modifiers_from(ctrl: bool, shift: bool, alt: bool, meta: bool, rest: [bool; 10]) -> Modifiers {
    let [
        alt_graph,
        caps_lock,
        fn_,
        fn_lock,
        hyper,
        num_lock,
        scroll_lock,
        super_,
        symbol,
        symbol_lock,
    ] = rest;
    Modifiers {
        ctrl,
        shift,
        alt,
        meta,
        alt_graph,
        caps_lock,
        fn_,
        fn_lock,
        hyper,
        num_lock,
        scroll_lock,
        super_,
        symbol,
        symbol_lock,
    }
}

macro_rules! modifier_rest {
    ($init:expr) => {
        [
            $init.modifier_alt_graph,
            $init.modifier_caps_lock,
            $init.modifier_fn,
            $init.modifier_fn_lock,
            $init.modifier_hyper,
            $init.modifier_num_lock,
            $init.modifier_scroll_lock,
            $init.modifier_super,
            $init.modifier_symbol,
            $init.modifier_symbol_lock,
        ]
    };
}

macro_rules! mouse_state {
    ($init:expr) => {
        UiEvent {
            has_view: $init.view.is_some(),
            detail: $init.detail,
            which: $init.which,
            modifiers: modifiers_from(
                $init.ctrl_key,
                $init.shift_key,
                $init.alt_key,
                $init.meta_key,
                modifier_rest!($init),
            ),
            screen: ($init.screen_x, $init.screen_y),
            client: ($init.client_x, $init.client_y),
            button: $init.button,
            buttons: $init.buttons,
            related_target: $init.related_target,
            ..UiEvent::default()
        }
    };
}

impl web::UIEventImpl for Web {
    fn view(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<WindowRef>> {
        ui(cx, this, |s| s.has_view.then_some(WindowRef))
    }

    fn detail(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.detail)
    }

    fn which(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        ui(cx, this, |s| s.which)
    }

    fn constructor(cx: &mut Cx<'_>, type_: String, init: web::UIEventInit) -> Fallible<ObjectId> {
        let state = UiEvent {
            has_view: init.view.is_some(),
            detail: init.detail,
            which: init.which,
            ..UiEvent::default()
        };
        Ok(make(
            cx,
            InterfaceId::UIEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            state,
        ))
    }
}

impl web::FocusEventImpl for Web {
    fn related_target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<EventTargetRef>> {
        ui(cx, this, |s| s.related_target)
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::FocusEventInit,
    ) -> Fallible<ObjectId> {
        let state = UiEvent {
            has_view: init.view.is_some(),
            detail: init.detail,
            which: init.which,
            related_target: init.related_target,
            ..UiEvent::default()
        };
        Ok(make(
            cx,
            InterfaceId::FocusEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            state,
        ))
    }
}

impl web::InputEventImpl for Web {
    fn data(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        ui(cx, this, |s| s.input.data.clone())
    }

    fn is_composing(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.input.is_composing)
    }

    fn input_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ui(cx, this, |s| s.input.input_type.clone())
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::InputEventInit,
    ) -> Fallible<ObjectId> {
        let state = UiEvent {
            has_view: init.view.is_some(),
            detail: init.detail,
            which: init.which,
            input: Input {
                data: init.data,
                is_composing: init.is_composing,
                input_type: init.input_type,
            },
            ..UiEvent::default()
        };
        Ok(make(
            cx,
            InterfaceId::InputEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            state,
        ))
    }
}

impl web::KeyboardEventImpl for Web {
    fn key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ui(cx, this, |s| s.key.key.clone())
    }

    fn code(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ui(cx, this, |s| s.key.code.clone())
    }

    fn location(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        ui(cx, this, |s| s.key.location)
    }

    fn ctrl_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.ctrl)
    }

    fn shift_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.shift)
    }

    fn alt_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.alt)
    }

    fn meta_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.meta)
    }

    fn repeat(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.key.repeat)
    }

    fn is_composing(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.key.is_composing)
    }

    fn get_modifier_state(cx: &mut Cx<'_>, this: ObjectId, key_arg: String) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.state(&key_arg))
    }

    fn char_code(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        ui(cx, this, |s| s.key.char_code)
    }

    fn key_code(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        ui(cx, this, |s| s.key.key_code)
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::KeyboardEventInit,
    ) -> Fallible<ObjectId> {
        let state = UiEvent {
            has_view: init.view.is_some(),
            detail: init.detail,
            which: init.which,
            modifiers: modifiers_from(
                init.ctrl_key,
                init.shift_key,
                init.alt_key,
                init.meta_key,
                modifier_rest!(init),
            ),
            key: Keyboard {
                key: init.key,
                code: init.code,
                location: init.location,
                repeat: init.repeat,
                is_composing: init.is_composing,
                char_code: init.char_code,
                key_code: init.key_code,
            },
            ..UiEvent::default()
        };
        Ok(make(
            cx,
            InterfaceId::KeyboardEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            state,
        ))
    }
}

impl web::MouseEventImpl for Web {
    fn screen_x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.screen.0)
    }

    fn screen_y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.screen.1)
    }

    fn client_x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.client.0)
    }

    fn client_y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.client.1)
    }

    fn page_x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| f64::from(s.client.0))
    }

    fn page_y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| f64::from(s.client.1))
    }

    fn x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| f64::from(s.client.0))
    }

    fn y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| f64::from(s.client.1))
    }

    fn offset_x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| f64::from(s.client.0))
    }

    fn offset_y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| f64::from(s.client.1))
    }

    fn ctrl_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.ctrl)
    }

    fn shift_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.shift)
    }

    fn alt_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.alt)
    }

    fn meta_key(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.meta)
    }

    fn button(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i16> {
        ui(cx, this, |s| s.button)
    }

    fn buttons(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        ui(cx, this, |s| s.buttons)
    }

    fn related_target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<EventTargetRef>> {
        ui(cx, this, |s| s.related_target)
    }

    fn get_modifier_state(cx: &mut Cx<'_>, this: ObjectId, key_arg: String) -> Fallible<bool> {
        ui(cx, this, |s| s.modifiers.state(&key_arg))
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::MouseEventInit,
    ) -> Fallible<ObjectId> {
        let flags = (init.bubbles, init.cancelable, init.composed);
        let state = mouse_state!(init);
        Ok(make(cx, InterfaceId::MouseEvent, type_, flags, state))
    }
}

impl web::WheelEventImpl for Web {
    fn delta_x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| s.wheel.delta.0)
    }

    fn delta_y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| s.wheel.delta.1)
    }

    fn delta_z(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| s.wheel.delta.2)
    }

    fn delta_mode(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        ui(cx, this, |s| s.wheel.delta_mode)
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::WheelEventInit,
    ) -> Fallible<ObjectId> {
        let flags = (init.bubbles, init.cancelable, init.composed);
        let wheel = Wheel {
            delta: (init.delta_x, init.delta_y, init.delta_z),
            delta_mode: init.delta_mode,
        };
        let mut state = mouse_state!(init);
        state.wheel = wheel;
        Ok(make(cx, InterfaceId::WheelEvent, type_, flags, state))
    }
}

impl web::PointerEventImpl for Web {
    fn pointer_id(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.pointer.pointer_id)
    }

    fn width(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| s.pointer.width)
    }

    fn height(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| s.pointer.height)
    }

    fn pressure(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| s.pointer.pressure)
    }

    fn tangential_pressure(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ui(cx, this, |s| s.pointer.tangential_pressure)
    }

    fn tilt_x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.pointer.tilt.0)
    }

    fn tilt_y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.pointer.tilt.1)
    }

    fn twist(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<i32> {
        ui(cx, this, |s| s.pointer.twist)
    }

    fn pointer_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ui(cx, this, |s| s.pointer.pointer_type.clone())
    }

    fn is_primary(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ui(cx, this, |s| s.pointer.is_primary)
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::PointerEventInit,
    ) -> Fallible<ObjectId> {
        let flags = (init.bubbles, init.cancelable, init.composed);
        let pointer = Pointer {
            pointer_id: init.pointer_id,
            width: init.width,
            height: init.height,
            pressure: init.pressure,
            tangential_pressure: init.tangential_pressure,
            tilt: (init.tilt_x.unwrap_or(0), init.tilt_y.unwrap_or(0)),
            twist: init.twist,
            pointer_type: init.pointer_type,
            is_primary: init.is_primary,
        };
        let mut state = mouse_state!(init);
        state.pointer = pointer;
        Ok(make(cx, InterfaceId::PointerEvent, type_, flags, state))
    }
}
