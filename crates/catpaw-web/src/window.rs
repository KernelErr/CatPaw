//! `Window` and the objects reachable from it that have no tree of their
//! own: `Location`, `Navigator`, `Performance`, `Storage`.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use catpaw_dom::NodeId;
use catpaw_js::{Callback, Exception, Fallible, ObjectId, PromiseRef, Value, WindowRef};
use url::Url;

use crate::event_loop::{self, TimerAction};
use crate::generated::{self as web, StringOrFunction};
use crate::page::{ConsoleLevel, Cx, DialogRecord, NavigationRequest, PageState, Singletons};
use crate::{Web, platform_object};

pub struct LocationObject;
platform_object!(LocationObject, Location);

pub struct NavigatorObject;
platform_object!(NavigatorObject, Navigator);

pub struct PerformanceObject;
platform_object!(PerformanceObject, Performance);

/// `localStorage` (area 0) or `sessionStorage` (area 1).
pub struct StorageObject {
    area: usize,
}
platform_object!(StorageObject, Storage);

/// Returns the page-wide object in `slot`, creating and pinning it on first
/// use so that script always sees the same object.
fn singleton(
    cx: &mut Cx<'_>,
    slot: fn(&mut Singletons) -> &mut Option<ObjectId>,
    make: impl FnOnce(&PageState) -> ObjectId,
) -> ObjectId {
    if let Some(id) = *slot(&mut cx.page.singletons.borrow_mut()) {
        return id;
    }
    let id = make(cx.page);
    *slot(&mut cx.page.singletons.borrow_mut()) = Some(id);
    cx.pin(id);
    id
}

pub(crate) fn location(cx: &mut Cx<'_>) -> ObjectId {
    singleton(cx, |s| &mut s.location, |page| page.alloc(LocationObject))
}

fn resolved_promise(cx: &mut Cx<'_>) -> PromiseRef {
    let promise = cx.script.new_promise();
    cx.script.resolve_promise(&promise, Value::Undefined);
    promise
}

fn dialog(cx: &mut Cx<'_>, kind: &'static str, message: String) {
    cx.page
        .log(ConsoleLevel::Info, format!("[{kind}] {message}"));
    cx.page
        .dialogs
        .borrow_mut()
        .push(DialogRecord { kind, message });
}

impl web::WindowImpl for Web {
    fn inner_width(cx: &mut Cx<'_>) -> Fallible<i32> {
        Ok(cx.page.config.viewport_width as i32)
    }

    fn inner_height(cx: &mut Cx<'_>) -> Fallible<i32> {
        Ok(cx.page.config.viewport_height as i32)
    }

    fn scroll_x(cx: &mut Cx<'_>) -> Fallible<f64> {
        Ok(cx.page.document_state.borrow().scroll_x)
    }

    fn page_x_offset(cx: &mut Cx<'_>) -> Fallible<f64> {
        Ok(cx.page.document_state.borrow().scroll_x)
    }

    fn scroll_y(cx: &mut Cx<'_>) -> Fallible<f64> {
        Ok(cx.page.document_state.borrow().scroll_y)
    }

    fn page_y_offset(cx: &mut Cx<'_>) -> Fallible<f64> {
        Ok(cx.page.document_state.borrow().scroll_y)
    }

    // Without layout the document has no scrollable overflow, so every
    // scroll request is a no-op that completes immediately.
    fn scroll(cx: &mut Cx<'_>, _options: web::ScrollToOptions) -> Fallible<PromiseRef> {
        Ok(resolved_promise(cx))
    }

    fn scroll_overload2(cx: &mut Cx<'_>, _x: f64, _y: f64) -> Fallible<PromiseRef> {
        Ok(resolved_promise(cx))
    }

    fn scroll_to(cx: &mut Cx<'_>, _options: web::ScrollToOptions) -> Fallible<PromiseRef> {
        Ok(resolved_promise(cx))
    }

    fn scroll_to_overload2(cx: &mut Cx<'_>, _x: f64, _y: f64) -> Fallible<PromiseRef> {
        Ok(resolved_promise(cx))
    }

    fn scroll_by(cx: &mut Cx<'_>, _options: web::ScrollToOptions) -> Fallible<PromiseRef> {
        Ok(resolved_promise(cx))
    }

    fn scroll_by_overload2(cx: &mut Cx<'_>, _x: f64, _y: f64) -> Fallible<PromiseRef> {
        Ok(resolved_promise(cx))
    }

    fn device_pixel_ratio(cx: &mut Cx<'_>) -> Fallible<f64> {
        Ok(cx.page.config.device_pixel_ratio)
    }

    fn event(cx: &mut Cx<'_>) -> Fallible<Option<ObjectId>> {
        Ok(cx.page.current_event.get())
    }

    fn window(_cx: &mut Cx<'_>) -> Fallible<WindowRef> {
        Ok(WindowRef)
    }

    fn self_(_cx: &mut Cx<'_>) -> Fallible<WindowRef> {
        Ok(WindowRef)
    }

    fn document(cx: &mut Cx<'_>) -> Fallible<NodeId> {
        Ok(cx.document())
    }

    fn location(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(location(cx))
    }

    fn history(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(singleton(
            cx,
            |s| &mut s.history,
            |page| page.alloc(crate::history::HistoryObject),
        ))
    }

    fn closed(_cx: &mut Cx<'_>) -> Fallible<bool> {
        Ok(false)
    }

    fn focus(_cx: &mut Cx<'_>) -> Fallible<()> {
        Ok(())
    }

    fn blur(_cx: &mut Cx<'_>) -> Fallible<()> {
        Ok(())
    }

    fn frames(_cx: &mut Cx<'_>) -> Fallible<WindowRef> {
        Ok(WindowRef)
    }

    fn top(_cx: &mut Cx<'_>) -> Fallible<Option<WindowRef>> {
        Ok(Some(WindowRef))
    }

    fn parent(_cx: &mut Cx<'_>) -> Fallible<Option<WindowRef>> {
        Ok(Some(WindowRef))
    }

    fn navigator(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(singleton(
            cx,
            |s| &mut s.navigator,
            |page| page.alloc(NavigatorObject),
        ))
    }

    // Dialogs never block: they are recorded and answered at once. An alert
    // is acknowledged; confirm and prompt are dismissed.
    fn alert(cx: &mut Cx<'_>) -> Fallible<()> {
        dialog(cx, "alert", String::new());
        Ok(())
    }

    fn alert_overload2(cx: &mut Cx<'_>, message: String) -> Fallible<()> {
        dialog(cx, "alert", message);
        Ok(())
    }

    fn confirm(cx: &mut Cx<'_>, message: String) -> Fallible<bool> {
        dialog(cx, "confirm", message);
        Ok(false)
    }

    fn prompt(cx: &mut Cx<'_>, message: String, _default: String) -> Fallible<Option<String>> {
        dialog(cx, "prompt", message);
        Ok(None)
    }
}

/// The forgiving-base64 decoder: padding is optional and stray trailing
/// bits are discarded.
const FORGIVING: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

/// <https://infra.spec.whatwg.org/#forgiving-base64-decode>
pub(crate) fn forgiving_base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut data: String = input
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r'))
        .collect();
    if data.len().is_multiple_of(4) {
        for _ in 0..2 {
            if data.ends_with('=') {
                data.pop();
            }
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    if !data
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return None;
    }
    FORGIVING.decode(data).ok()
}

fn is_secure(url: &Url) -> bool {
    match url.scheme() {
        "https" | "wss" | "file" => true,
        _ => match url.host() {
            Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
    }
}

fn timer_action(handler: StringOrFunction, arguments: Vec<Value>) -> TimerAction {
    match handler {
        StringOrFunction::Function(callback) => TimerAction::Call(callback, arguments),
        StringOrFunction::String(source) => TimerAction::Eval(source),
    }
}

impl web::WindowOrWorkerGlobalScopeImpl for Web {
    fn performance(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(singleton(
            cx,
            |s| &mut s.performance,
            |page| page.alloc(PerformanceObject),
        ))
    }

    fn origin(cx: &mut Cx<'_>) -> Fallible<String> {
        Ok(cx.page.url.borrow().origin().ascii_serialization())
    }

    fn is_secure_context(cx: &mut Cx<'_>) -> Fallible<bool> {
        Ok(is_secure(&cx.page.url.borrow()))
    }

    fn report_error(cx: &mut Cx<'_>, e: Value) -> Fallible<()> {
        let text = format!("Uncaught {}", cx.script.display(std::slice::from_ref(&e)));
        cx.page.errors.borrow_mut().push(text.clone());
        cx.page.log(ConsoleLevel::Error, text);
        Ok(())
    }

    fn btoa(_cx: &mut Cx<'_>, data: String) -> Fallible<String> {
        let mut bytes = Vec::with_capacity(data.len());
        for c in data.chars() {
            let code = c as u32;
            if code > 0xFF {
                return Err(Exception::invalid_character(
                    "The string contains characters outside of the Latin1 range",
                ));
            }
            bytes.push(code as u8);
        }
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    fn atob(_cx: &mut Cx<'_>, data: String) -> Fallible<String> {
        let bytes = forgiving_base64_decode(&data).ok_or_else(|| {
            Exception::invalid_character("The string is not correctly base64 encoded")
        })?;
        Ok(bytes.into_iter().map(char::from).collect())
    }

    fn set_timeout(
        cx: &mut Cx<'_>,
        handler: StringOrFunction,
        timeout: i32,
        arguments: Vec<Value>,
    ) -> Fallible<i32> {
        let action = timer_action(handler, arguments);
        Ok(event_loop::set_timer(cx.page, action, timeout, false))
    }

    fn clear_timeout(cx: &mut Cx<'_>, id: i32) -> Fallible<()> {
        event_loop::clear_timer(cx.page, id);
        Ok(())
    }

    fn set_interval(
        cx: &mut Cx<'_>,
        handler: StringOrFunction,
        timeout: i32,
        arguments: Vec<Value>,
    ) -> Fallible<i32> {
        let action = timer_action(handler, arguments);
        Ok(event_loop::set_timer(cx.page, action, timeout, true))
    }

    fn clear_interval(cx: &mut Cx<'_>, id: i32) -> Fallible<()> {
        event_loop::clear_timer(cx.page, id);
        Ok(())
    }

    fn queue_microtask(cx: &mut Cx<'_>, callback: Callback) -> Fallible<()> {
        cx.script.queue_microtask(callback);
        Ok(())
    }

    fn structured_clone(
        cx: &mut Cx<'_>,
        value: Value,
        _options: web::StructuredSerializeOptions,
    ) -> Fallible<Value> {
        cx.script.structured_clone(&value)
    }
}

impl web::AnimationFrameProviderImpl for Web {
    fn request_animation_frame(cx: &mut Cx<'_>, callback: Callback) -> Fallible<u32> {
        Ok(event_loop::request_animation_frame(cx.page, callback))
    }

    fn cancel_animation_frame(cx: &mut Cx<'_>, handle: u32) -> Fallible<()> {
        event_loop::cancel_animation_frame(cx.page, handle);
        Ok(())
    }
}

impl web::WindowLocalStorageImpl for Web {
    fn local_storage(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(singleton(
            cx,
            |s| &mut s.local_storage,
            |page| page.alloc(StorageObject { area: 0 }),
        ))
    }
}

impl web::WindowSessionStorageImpl for Web {
    fn session_storage(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(singleton(
            cx,
            |s| &mut s.session_storage,
            |page| page.alloc(StorageObject { area: 1 }),
        ))
    }
}

// ---- Location -------------------------------------------------------------

/// Navigates to `url`: within the document when only the fragment differs,
/// otherwise by asking the embedder to load it.
pub(crate) fn navigate(cx: &mut Cx<'_>, url: Url, replace: bool) {
    let current = cx.page.url.borrow().clone();
    let same_document = {
        let (mut a, mut b) = (url.clone(), current.clone());
        a.set_fragment(None);
        b.set_fragment(None);
        a == b && url.fragment().is_some()
    };
    if !same_document {
        *cx.page.navigation.borrow_mut() = Some(NavigationRequest {
            url,
            replace,
            reload: false,
        });
        return;
    }
    if url == current {
        return;
    }
    crate::history::commit_entry(cx, url.clone(), Value::Null, replace);
    event_loop::queue_task(cx.page, "hashchange", move |cx| {
        crate::history::fire_hashchange(cx, &current, &url);
    });
}

fn navigate_to(cx: &mut Cx<'_>, input: &str, replace: bool) -> Fallible<()> {
    let url = cx
        .page
        .resolve_url(input)
        .ok_or_else(|| Exception::syntax(format!("'{input}' is not a valid URL")))?;
    navigate(cx, url, replace);
    Ok(())
}

/// Applies `edit` to a copy of the document URL and navigates to the result.
fn navigate_edited(cx: &mut Cx<'_>, edit: impl FnOnce(&mut Url)) -> Fallible<()> {
    let mut url = cx.page.url.borrow().clone();
    edit(&mut url);
    navigate(cx, url, false);
    Ok(())
}

fn current<R>(cx: &Cx<'_>, f: impl FnOnce(&Url) -> R) -> Fallible<R> {
    Ok(f(&cx.page.url.borrow()))
}

impl web::LocationImpl for Web {
    fn href(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| u.to_string())
    }

    fn set_href(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_to(cx, &value, false)
    }

    fn origin(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, url::quirks::origin)
    }

    fn protocol(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| url::quirks::protocol(u).to_string())
    }

    fn set_protocol(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_edited(cx, |u| {
            let _ = url::quirks::set_protocol(u, &value);
        })
    }

    fn host(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| url::quirks::host(u).to_string())
    }

    fn set_host(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_edited(cx, |u| {
            let _ = url::quirks::set_host(u, &value);
        })
    }

    fn hostname(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| url::quirks::hostname(u).to_string())
    }

    fn set_hostname(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_edited(cx, |u| {
            let _ = url::quirks::set_hostname(u, &value);
        })
    }

    fn port(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| url::quirks::port(u).to_string())
    }

    fn set_port(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_edited(cx, |u| {
            let _ = url::quirks::set_port(u, &value);
        })
    }

    fn pathname(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| url::quirks::pathname(u).to_string())
    }

    fn set_pathname(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_edited(cx, |u| url::quirks::set_pathname(u, &value))
    }

    fn search(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| url::quirks::search(u).to_string())
    }

    fn set_search(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_edited(cx, |u| url::quirks::set_search(u, &value))
    }

    fn hash(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        current(cx, |u| url::quirks::hash(u).to_string())
    }

    fn set_hash(cx: &mut Cx<'_>, _this: ObjectId, value: String) -> Fallible<()> {
        navigate_edited(cx, |u| {
            // Setting the hash always leaves a fragment, even an empty one,
            // so that the navigation stays within the document.
            let fragment = value.strip_prefix('#').unwrap_or(&value);
            u.set_fragment(Some(fragment));
        })
    }

    fn assign(cx: &mut Cx<'_>, _this: ObjectId, url: String) -> Fallible<()> {
        navigate_to(cx, &url, false)
    }

    fn replace(cx: &mut Cx<'_>, _this: ObjectId, url: String) -> Fallible<()> {
        navigate_to(cx, &url, true)
    }

    fn reload(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        let url = cx.page.url.borrow().clone();
        *cx.page.navigation.borrow_mut() = Some(NavigationRequest {
            url,
            replace: true,
            reload: true,
        });
        Ok(())
    }
}

// ---- Navigator ------------------------------------------------------------

impl web::NavigatorIDImpl for Web {
    fn app_code_name(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("Mozilla".to_string())
    }

    fn app_name(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("Netscape".to_string())
    }

    fn app_version(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        let ua = &cx.page.config.user_agent;
        Ok(ua.strip_prefix("Mozilla/").unwrap_or(ua).to_string())
    }

    fn platform(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok(cx.page.config.platform.clone())
    }

    fn product(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("Gecko".to_string())
    }

    fn product_sub(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("20030107".to_string())
    }

    fn user_agent(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok(cx.page.config.user_agent.clone())
    }

    fn vendor(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok(String::new())
    }

    fn vendor_sub(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok(String::new())
    }
}

impl web::NavigatorLanguageImpl for Web {
    fn language(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok(cx
            .page
            .config
            .languages
            .first()
            .cloned()
            .unwrap_or_else(|| "en-US".to_string()))
    }

    fn languages(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Vec<String>> {
        Ok(cx.page.config.languages.clone())
    }
}

impl web::NavigatorOnLineImpl for Web {
    fn on_line(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<bool> {
        Ok(true)
    }
}

impl web::NavigatorCookiesImpl for Web {
    fn cookie_enabled(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<bool> {
        Ok(true)
    }
}

impl web::NavigatorConcurrentHardwareImpl for Web {
    fn hardware_concurrency(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u64> {
        Ok(u64::from(cx.page.config.hardware_concurrency))
    }
}

impl web::NavigatorAutomationInformationImpl for Web {
    /// This browser is always driven by automation, and says so.
    fn webdriver(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<bool> {
        Ok(true)
    }
}

// ---- Performance ----------------------------------------------------------

impl web::PerformanceImpl for Web {
    fn now(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<f64> {
        Ok(cx.page.clock.now())
    }

    fn time_origin(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<f64> {
        Ok(cx.page.clock.time_origin())
    }
}

// ---- Storage --------------------------------------------------------------

fn area(cx: &Cx<'_>, this: ObjectId) -> Fallible<usize> {
    cx.page.with::<StorageObject, _>(this, |s| s.area)
}

impl web::StorageImpl for Web {
    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        let area = area(cx, this)?;
        Ok(cx.page.storage[area].borrow().len() as u32)
    }

    fn key(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<String>> {
        let area = area(cx, this)?;
        Ok(cx.page.storage[area]
            .borrow()
            .get_index(index as usize)
            .map(|(k, _)| k.clone()))
    }

    fn get_item(cx: &mut Cx<'_>, this: ObjectId, key: String) -> Fallible<Option<String>> {
        let area = area(cx, this)?;
        Ok(cx.page.storage[area].borrow().get(&key).cloned())
    }

    fn set_item(cx: &mut Cx<'_>, this: ObjectId, key: String, value: String) -> Fallible<()> {
        let area = area(cx, this)?;
        cx.page.storage[area].borrow_mut().insert(key, value);
        Ok(())
    }

    fn remove_item(cx: &mut Cx<'_>, this: ObjectId, key: String) -> Fallible<()> {
        let area = area(cx, this)?;
        cx.page.storage[area].borrow_mut().shift_remove(&key);
        Ok(())
    }

    fn clear(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let area = area(cx, this)?;
        cx.page.storage[area].borrow_mut().clear();
        Ok(())
    }

    fn named_get(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<Option<String>> {
        let area = area(cx, this)?;
        Ok(cx.page.storage[area].borrow().get(name).cloned())
    }

    fn named_properties(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<String>> {
        let area = area(cx, this)?;
        Ok(cx.page.storage[area].borrow().keys().cloned().collect())
    }

    fn named_set(cx: &mut Cx<'_>, this: ObjectId, name: &str, value: String) -> Fallible<()> {
        let area = area(cx, this)?;
        cx.page.storage[area]
            .borrow_mut()
            .insert(name.to_string(), value);
        Ok(())
    }

    fn named_delete(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<bool> {
        let area = area(cx, this)?;
        cx.page.storage[area].borrow_mut().shift_remove(name);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgiving_base64() {
        assert_eq!(forgiving_base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(forgiving_base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(forgiving_base64_decode(" aGVs\nbG8= ").unwrap(), b"hello");
        // Non-zero trailing bits are discarded rather than rejected.
        assert_eq!(forgiving_base64_decode("YR").unwrap(), b"a");
        assert_eq!(forgiving_base64_decode("").unwrap(), b"");
        assert!(forgiving_base64_decode("a").is_none());
        assert!(forgiving_base64_decode("ab=").is_none());
        assert!(forgiving_base64_decode("a!bc").is_none());
    }

    #[test]
    fn secure_contexts() {
        let secure = |s: &str| is_secure(&Url::parse(s).unwrap());
        assert!(secure("https://example.com/"));
        assert!(secure("http://localhost:8080/"));
        assert!(secure("http://127.0.0.1/"));
        assert!(!secure("http://example.com/"));
    }
}
