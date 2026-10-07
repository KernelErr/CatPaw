//! Script elements and the document parser
//! (<https://html.spec.whatwg.org/multipage/#prepare-the-script-element>).
//!
//! [`load_document`] drives an [`HtmlStream`]: the parser stops at every
//! `</script>`, the script runs (and may `document.write` more markup at the
//! insertion point), and parsing resumes. Scripts inserted by other scripts
//! are prepared from the tree-mutation hooks at the bottom of this module.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use catpaw_dom::{HtmlParseOptions, HtmlStream, NodeId, TreeChange, WriteQueue};
use catpaw_js::EventTargetRef;
use encoding_rs::Encoding;
use url::Url;

use crate::element::child_text_content;
use crate::event_loop::queue_task;
use crate::events;
use crate::generated::DocumentReadyState;
use crate::mutation_observer;
use crate::navigation_timing;
use crate::net::{self, NetRequest, NetResult, RequestKind};
use crate::page::{ConsoleLevel, Cx, PageState};
use crate::stylesheets;

/// Script-loading state of a page.
#[derive(Default)]
pub struct ScriptState {
    /// The parser of the document being loaded or rewritten.
    parser: RefCell<Option<Rc<HtmlStream>>>,
    /// The active parser was created by `document.open()`.
    script_created_parser: Cell<bool>,
    /// How many parser-inserted scripts are running (nested through
    /// `document.write`). While positive, `document.write` has an insertion
    /// point.
    parser_script_depth: Cell<u32>,
    /// `defer` scripts, in document order, waiting for the end of parsing.
    deferred: RefCell<Vec<Deferred>>,
    /// Fetches that must finish before the `load` event.
    load_blockers: Cell<u32>,
    /// Parsing has ended; `load` fires once nothing blocks it.
    load_pending: Cell<bool>,
    /// The page's import map: specifier (or specifier prefix ending in `/`)
    /// to address.
    import_map: RefCell<Vec<(String, String)>>,
}

impl ScriptState {
    /// Whether the document's `load` event is still to come.
    pub fn is_loading(&self) -> bool {
        self.parser.borrow().is_some() || self.load_pending.get()
    }
}

/// Where the source of a script waiting for the end of parsing comes from.
enum Pending {
    /// An external script being fetched.
    Fetch(Rc<RefCell<Option<NetResult>>>),
    /// An inline module script (inline classic scripts never wait).
    Inline(String),
}

struct Deferred {
    element: NodeId,
    url: Url,
    module: bool,
    pending: Pending,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScriptKind {
    Classic,
    Module,
    ImportMap,
    /// A data block: not executed.
    Data,
}

const JAVASCRIPT_MIME_TYPES: &[&str] = &[
    "application/ecmascript",
    "application/javascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
];

fn classify(type_attr: Option<&str>, language_attr: Option<&str>) -> ScriptKind {
    let ty = match (type_attr, language_attr) {
        (Some(t), _) if t.trim().is_empty() => "text/javascript".to_string(),
        (Some(t), _) => t.trim().to_ascii_lowercase(),
        (None, Some(l)) if !l.is_empty() => format!("text/{}", l.to_ascii_lowercase()),
        _ => "text/javascript".to_string(),
    };
    if JAVASCRIPT_MIME_TYPES.contains(&ty.as_str()) {
        ScriptKind::Classic
    } else if ty == "module" {
        ScriptKind::Module
    } else if ty == "importmap" {
        ScriptKind::ImportMap
    } else {
        ScriptKind::Data
    }
}

/// Whether a specifier is "URL-like": resolved against the importing
/// module rather than looked up by name.
fn is_relative_specifier(specifier: &str) -> bool {
    specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../")
}

/// Registers the `imports` of an import map (scopes and integrity are not
/// supported). Later maps add to earlier ones without overriding them.
fn register_import_map(cx: &mut Cx<'_>, json: &str) {
    let parsed: serde_json::Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(e) => {
            cx.page.log(
                ConsoleLevel::Error,
                format!("Failed to parse the import map: {e}"),
            );
            return;
        }
    };
    let Some(imports) = parsed.get("imports").and_then(|i| i.as_object()) else {
        return;
    };
    let base = cx.page.base_url();
    let mut map = cx.page.scripts.import_map.borrow_mut();
    for (key, address) in imports {
        let Some(address) = address.as_str() else {
            continue;
        };
        let key = if is_relative_specifier(key) {
            match base.join(key) {
                Ok(url) => url.to_string(),
                Err(_) => continue,
            }
        } else {
            key.clone()
        };
        let address = if is_relative_specifier(address) {
            base.join(address).map(|u| u.to_string()).ok()
        } else {
            Url::parse(address).map(|u| u.to_string()).ok()
        };
        if let Some(address) = address
            && !map.iter().any(|(k, _)| *k == key)
        {
            map.push((key, address));
        }
    }
}

/// Resolves a module specifier imported from the module at `base`
/// (<https://html.spec.whatwg.org/multipage/#resolve-a-module-specifier>).
pub fn resolve_module_specifier(
    page: &crate::page::PageState,
    specifier: &str,
    base: &Url,
) -> Option<Url> {
    let as_url = if is_relative_specifier(specifier) {
        base.join(specifier).ok()
    } else {
        Url::parse(specifier).ok()
    };
    let normalized = as_url
        .as_ref()
        .map(Url::to_string)
        .unwrap_or_else(|| specifier.to_string());

    let map = page.scripts.import_map.borrow();
    if let Some((_, address)) = map.iter().find(|(key, _)| *key == normalized) {
        return Url::parse(address).ok();
    }
    // The longest matching prefix entry (`"lib/": "https://cdn.example/lib/"`).
    let prefix = map
        .iter()
        .filter(|(key, address)| {
            key.ends_with('/') && address.ends_with('/') && normalized.starts_with(key.as_str())
        })
        .max_by_key(|(key, _)| key.len());
    if let Some((key, address)) = prefix {
        return Url::parse(address)
            .ok()?
            .join(&normalized[key.len()..])
            .ok();
    }
    as_url
}

/// Decodes a fetched script or style sheet: the BOM wins, then the
/// Content-Type charset, then UTF-8.
pub(crate) fn decode_text(body: &[u8], content_type: Option<&str>) -> String {
    let charset = content_type.and_then(|ct| {
        ct.split(';').skip(1).find_map(|param| {
            let (name, value) = param.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches('"').to_string())
        })
    });
    let encoding = charset
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(body).0.into_owned()
}

fn fire_simple(cx: &mut Cx<'_>, el: NodeId, type_: &'static str) {
    if cx.dom().contains(el) {
        events::fire(cx, EventTargetRef::Node(el), type_, false, false);
    }
}

/// Runs a classic script and reports an uncaught exception.
/// Runs a classic script whose source starts on line `line` of `url`
/// (inline scripts start where their element does).
fn execute(cx: &mut Cx<'_>, el: NodeId, source: &str, url: &str, parser_inserted: bool, line: u32) {
    let scripts = &cx.page.scripts;
    let previous = cx
        .page
        .document_state
        .borrow_mut()
        .current_script
        .replace(el);
    if parser_inserted {
        scripts
            .parser_script_depth
            .set(scripts.parser_script_depth.get() + 1);
    }
    let result = cx.script.eval_script(source, url, line);
    if parser_inserted {
        scripts
            .parser_script_depth
            .set(scripts.parser_script_depth.get() - 1);
    }
    cx.page.document_state.borrow_mut().current_script = previous;
    if let Err(e) = result {
        cx.report_exception(&e);
    }
}

/// Runs a module script. Returns whether it loaded and evaluated.
fn execute_module(cx: &mut Cx<'_>, source: &str, url: &str) -> bool {
    // `document.currentScript` is null while a module runs.
    let previous = cx.page.document_state.borrow_mut().current_script.take();
    let result = cx.script.eval_module(source, url);
    cx.page.document_state.borrow_mut().current_script = previous;
    match result {
        Ok(()) => true,
        Err(e) => {
            cx.report_exception(&e);
            false
        }
    }
}

/// Runs the body of a fetched script, or fires `error` if the fetch failed.
fn execute_fetched(
    cx: &mut Cx<'_>,
    el: NodeId,
    url: &Url,
    result: NetResult,
    parser_inserted: bool,
    module: bool,
) {
    match result {
        Ok(response) if response.is_success() && module => {
            // Module scripts are always UTF-8.
            let source = String::from_utf8_lossy(&response.body);
            let source = source.strip_prefix('\u{FEFF}').unwrap_or(&source);
            if execute_module(cx, source, response.url.as_str()) {
                fire_simple(cx, el, "load");
            } else {
                fire_simple(cx, el, "error");
            }
        }
        Ok(response) if response.is_success() => {
            let source = decode_text(&response.body, response.header("content-type"));
            execute(cx, el, &source, response.url.as_str(), parser_inserted, 1);
            fire_simple(cx, el, "load");
        }
        Ok(response) => {
            cx.page.log(
                ConsoleLevel::Error,
                format!("Failed to load script {url}: HTTP {}", response.status),
            );
            fire_simple(cx, el, "error");
        }
        Err(reason) => {
            cx.page.log(
                ConsoleLevel::Error,
                format!("Failed to load script {url}: {reason}"),
            );
            fire_simple(cx, el, "error");
        }
    }
}

/// Delays the document's `load` event until a matching [`unblock_load`].
pub fn block_load(page: &PageState) {
    let scripts = &page.scripts;
    scripts.load_blockers.set(scripts.load_blockers.get() + 1);
}

pub fn unblock_load(page: &PageState) {
    let scripts = &page.scripts;
    scripts
        .load_blockers
        .set(scripts.load_blockers.get().saturating_sub(1));
    maybe_fire_load(page);
}

/// Prepares a script element: decides whether and when it runs.
fn prepare(cx: &mut Cx<'_>, el: NodeId, parser_inserted: bool) {
    let (src, kind, is_async, is_defer) = {
        let mut dom = cx.dom_mut();
        let Some(data) = dom.element(el) else { return };
        if !data.is_html() || &*data.name.local != "script" || data.script_already_started {
            return;
        }
        let src = data.attr("src").map(str::to_string);
        let kind = classify(data.attr("type"), data.attr("language"));
        let flags = (data.has_attr("async"), data.has_attr("defer"));
        let nomodule = data.has_attr("nomodule");
        if src.is_none() && child_text_content(&dom, el).is_empty() {
            return;
        }
        if !dom.is_connected(el) {
            return;
        }
        // Data blocks never run, and neither do the fallbacks meant for
        // browsers without module support.
        if kind == ScriptKind::Data || (kind == ScriptKind::Classic && nomodule) {
            return;
        }
        if let Some(data) = dom.element_mut(el) {
            data.script_already_started = true;
        }
        (src, kind, flags.0, flags.1)
    };
    let module = kind == ScriptKind::Module;

    if kind == ScriptKind::ImportMap {
        if src.is_none() {
            let json = child_text_content(&cx.dom(), el);
            register_import_map(cx, &json);
        }
        return;
    }

    let Some(src) = src else {
        let source = child_text_content(&cx.dom(), el);
        let url = cx.page.url.borrow().clone();
        if !module {
            // The parser stands at the end tag: the source began as many
            // lines up as it has line breaks.
            let line = if parser_inserted {
                let parser = cx.page.scripts.parser.borrow().clone();
                parser.map_or(1, |p| {
                    let breaks = source.matches('\n').count() as u64;
                    p.current_line().saturating_sub(breaks).max(1) as u32
                })
            } else {
                1
            };
            execute(cx, el, &source, url.as_str(), parser_inserted, line);
        } else if parser_inserted && !is_async {
            // Module scripts are deferred, inline ones included.
            cx.page.scripts.deferred.borrow_mut().push(Deferred {
                element: el,
                url,
                module: true,
                pending: Pending::Inline(source),
            });
        } else {
            block_load(cx.page);
            queue_task(cx.page, "module script", move |cx| {
                execute_module(cx, &source, url.as_str());
                unblock_load(cx.page);
            });
        }
        return;
    };

    let url = (!src.is_empty())
        .then(|| cx.page.resolve_url(&src))
        .flatten();
    let Some(url) = url else {
        queue_task(cx.page, "script error", move |cx| {
            fire_simple(cx, el, "error")
        });
        return;
    };
    let mut request = NetRequest::get(url.clone(), RequestKind::Script);
    request.referrer = crate::referrer::for_document(cx.page, &url);

    if parser_inserted && !module && !is_async && !is_defer {
        // Parser-blocking: nothing else happens until it has run.
        let result = net::fetch_blocking(cx.page, request);
        execute_fetched(cx, el, &url, result, true, false);
        return;
    }

    if parser_inserted && !is_async && (module || is_defer) {
        // Fetched while parsing continues; run in order when it ends.
        let result = Rc::new(RefCell::new(None));
        let slot = result.clone();
        net::start_request(cx.page, request, move |_cx, outcome| {
            *slot.borrow_mut() = Some(outcome);
        });
        cx.page.scripts.deferred.borrow_mut().push(Deferred {
            element: el,
            url,
            module,
            pending: Pending::Fetch(result),
        });
        return;
    }

    // Async (and every script-inserted external script): run when fetched.
    block_load(cx.page);
    net::start_request(cx.page, request, move |cx, outcome| {
        execute_fetched(cx, el, &url, outcome, false, module);
        unblock_load(cx.page);
    });
}

fn run_parser_script(cx: &mut Cx<'_>, el: NodeId) {
    // A script sees the styles of the sheets that come before it.
    stylesheets::wait_for_blocking_sheets(cx);
    // Microtasks queued by earlier scripts run before the next one starts.
    cx.checkpoint();
    prepare(cx, el, true);
}

/// Runs the parser, then lets the rest of the page react to the tree
/// changes it made. Scripts do not run inside `run`.
fn parse<R>(page: &PageState, run: impl FnOnce() -> R) -> R {
    page.dom.borrow_mut().log_changes(true);
    let result = run();
    let changes = {
        let mut dom = page.dom.borrow_mut();
        let changes = dom.take_changes();
        dom.log_changes(false);
        changes
    };
    let mut inserted = Vec::new();
    for change in &changes {
        if let TreeChange::Inserted { node, .. } = *change {
            stylesheets::link_changed(page, node, true);
            crate::frames::iframe_changed(page, node);
            inserted.push(node);
        }
    }
    {
        let dom = page.dom.borrow();
        if inserted
            .iter()
            .any(|&n| crate::document::is_nameable(&dom, n))
        {
            page.document_names.changed();
        }
    }
    crate::custom_elements::parser_inserted(page, &inserted);
    mutation_observer::parser_changed(page, &changes);
    result
}

fn pump(cx: &mut Cx<'_>, stream: &HtmlStream) {
    while let Some(script) = parse(cx.page, || stream.pump()) {
        run_parser_script(cx, script);
        if cx.page.navigation.borrow().is_some() {
            return;
        }
    }
}

fn set_ready_state(cx: &mut Cx<'_>, state: DocumentReadyState) {
    cx.page.document_state.borrow_mut().ready_state = state;
    let timing = &cx.page.timing;
    match state {
        DocumentReadyState::Interactive => {
            navigation_timing::reached(cx.page, &timing.dom_interactive)
        }
        DocumentReadyState::Complete => navigation_timing::reached(cx.page, &timing.dom_complete),
        DocumentReadyState::Loading => {}
    }
    let document = cx.document();
    events::fire(
        cx,
        EventTargetRef::Node(document),
        "readystatechange",
        false,
        false,
    );
}

fn maybe_fire_load(page: &PageState) {
    let scripts = &page.scripts;
    if !scripts.load_pending.get() || scripts.load_blockers.get() > 0 {
        return;
    }
    scripts.load_pending.set(false);
    queue_task(page, "load", |cx| {
        set_ready_state(cx, DocumentReadyState::Complete);
        navigation_timing::reached(cx.page, &cx.page.timing.load_start);
        events::fire(cx, EventTargetRef::Window, "load", false, false);
        navigation_timing::reached(cx.page, &cx.page.timing.load_end);
        events::fire(cx, EventTargetRef::Window, "pageshow", false, false);
    });
}

/// <https://html.spec.whatwg.org/multipage/#the-end>
fn finish_parsing(cx: &mut Cx<'_>) {
    set_ready_state(cx, DocumentReadyState::Interactive);

    stylesheets::wait_for_blocking_sheets(cx);
    let deferred = std::mem::take(&mut *cx.page.scripts.deferred.borrow_mut());
    for script in deferred {
        match script.pending {
            Pending::Inline(source) => {
                cx.checkpoint();
                execute_module(cx, &source, script.url.as_str());
            }
            Pending::Fetch(slot) => {
                // Wait for this script's fetch; other fetches complete
                // meanwhile, and so do queued tasks (a data URL's answer
                // is one).
                while slot.borrow().is_none() {
                    if net::inflight(cx.page) > 0 {
                        net::deliver(cx, Some(Duration::from_millis(50)));
                    } else if !crate::event_loop::run_one_task(cx) {
                        break;
                    }
                }
                // Without a network the failure arrives as a task instead.
                if slot.borrow().is_none() && cx.page.net().is_none() {
                    *slot.borrow_mut() = Some(Err("no network available".to_string()));
                }
                let result = slot
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| Err("the request was abandoned".to_string()));
                cx.checkpoint();
                execute_fetched(
                    cx,
                    script.element,
                    &script.url,
                    result,
                    false,
                    script.module,
                );
            }
        }
        if cx.page.navigation.borrow().is_some() {
            return;
        }
    }

    let document = cx.document();
    navigation_timing::reached(cx.page, &cx.page.timing.dom_content_loaded_start);
    events::fire(
        cx,
        EventTargetRef::Node(document),
        "DOMContentLoaded",
        true,
        false,
    );
    navigation_timing::reached(cx.page, &cx.page.timing.dom_content_loaded_end);
    cx.page.scripts.load_pending.set(true);
    maybe_fire_load(cx.page);
}

fn new_stream(cx: &Cx<'_>) -> Rc<HtmlStream> {
    let options = HtmlParseOptions {
        scripting_enabled: true,
        iframe_srcdoc: false,
        url: Some(cx.page.url.borrow().clone()),
    };
    Rc::new(HtmlStream::new(cx.page.dom.clone(), &options))
}

/// Parses `html` as the page's document, running scripts as they are
/// encountered. On return the document is parsed and `DOMContentLoaded` has
/// fired; the `load` event follows from the event loop once async scripts
/// have finished.
pub fn load_document(cx: &mut Cx<'_>, html: &str) {
    let stream = new_stream(cx);
    *cx.page.scripts.parser.borrow_mut() = Some(stream.clone());
    let page = cx.page;
    crate::settle::with_initiator(page, crate::settle::Initiator::Parser, || {
        stream.push(html);
        pump(cx, &stream);
        parse(page, || stream.finish());
    });
    *cx.page.scripts.parser.borrow_mut() = None;
    if cx.page.navigation.borrow().is_some() {
        return;
    }
    finish_parsing(cx);
    cx.checkpoint();
}

/// `document.open()`: once the document has loaded, starts over with an
/// empty document and a fresh parser.
pub(crate) fn document_open(cx: &mut Cx<'_>) {
    if cx.page.scripts.parser.borrow().is_some() {
        return;
    }
    let document = cx.document();
    crate::node::replace_all(cx, None, document);
    let stream = new_stream(cx);
    *cx.page.scripts.parser.borrow_mut() = Some(stream);
    cx.page.scripts.script_created_parser.set(true);
    cx.page.document_state.borrow_mut().ready_state = DocumentReadyState::Loading;
}

/// `document.close()`: ends a parse started by `document.open()`.
pub(crate) fn document_close(cx: &mut Cx<'_>) {
    if !cx.page.scripts.script_created_parser.get() {
        return;
    }
    let stream = cx.page.scripts.parser.borrow_mut().take();
    cx.page.scripts.script_created_parser.set(false);
    if let Some(stream) = stream {
        parse(cx.page, || stream.finish());
        finish_parsing(cx);
    }
}

/// `document.write()`.
pub(crate) fn document_write(cx: &mut Cx<'_>, text: &str) {
    let scripts = &cx.page.scripts;
    if scripts.parser.borrow().is_none() {
        document_open(cx);
    } else if scripts.parser_script_depth.get() == 0 && !scripts.script_created_parser.get() {
        // The parser is active but this script is not running at its
        // insertion point (an async script, a timer): the write is ignored,
        // as it would otherwise wipe the document.
        cx.page.log(
            ConsoleLevel::Warn,
            "document.write() from an asynchronous script was ignored",
        );
        return;
    }
    let Some(stream) = cx.page.scripts.parser.borrow().clone() else {
        return;
    };
    let queue = WriteQueue::new(text);
    while let Some(script) = parse(cx.page, || stream.pump_write(&queue)) {
        run_parser_script(cx, script);
    }
}

// ---- tree-mutation hooks ---------------------------------------------------

/// Called after script inserted `inserted` under `parent`.
pub(crate) fn nodes_inserted(cx: &mut Cx<'_>, parent: NodeId, inserted: &[NodeId]) {
    let (scripts, links, named): (Vec<NodeId>, Vec<NodeId>, bool) = {
        let dom = cx.dom();
        if !dom.is_connected(parent) {
            return;
        }
        let mut scripts = Vec::new();
        let mut links = Vec::new();
        let mut named = false;
        // Content added to a script element that has not run yet.
        if dom.is_html_element(parent, "script") {
            scripts.push(parent);
        }
        for &node in inserted {
            for n in dom.traverse(node) {
                if dom.is_html_element(n, "script") {
                    scripts.push(n);
                } else if dom.is_html_element(n, "link") {
                    links.push(n);
                } else if crate::document::is_nameable(&dom, n) {
                    named = true;
                }
            }
        }
        (scripts, links, named)
    };
    if named {
        cx.page.document_names.changed();
    }
    crate::custom_elements::nodes_inserted(cx.page, inserted);
    crate::frames::nodes_inserted(cx.page, inserted);
    crate::forms::options_changed(cx.page, inserted);
    for link in links {
        stylesheets::link_changed(cx.page, link, false);
    }
    for script in scripts {
        prepare(cx, script, false);
    }
}

/// Called after a character data node's data changed.
pub(crate) fn character_data_changed(cx: &Cx<'_>, node: NodeId) {
    let script = {
        let dom = cx.dom();
        dom.parent(node)
            .filter(|&p| dom.is_html_element(p, "script") && dom.is_connected(p))
    };
    if let Some(script) = script {
        // Preparing may run the script; do it from a task rather than from
        // inside the mutation that triggered it.
        queue_task(cx.page, "script text changed", move |cx| {
            prepare(cx, script, false)
        });
    }
}

/// Called after the `src` attribute of a script element was set.
pub(crate) fn src_attribute_set(cx: &mut Cx<'_>, el: NodeId) {
    let connected = cx.dom().is_connected(el);
    if connected {
        prepare(cx, el, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_script_types() {
        assert_eq!(classify(None, None), ScriptKind::Classic);
        assert_eq!(classify(Some(""), None), ScriptKind::Classic);
        assert_eq!(
            classify(Some(" Text/JavaScript "), None),
            ScriptKind::Classic
        );
        assert_eq!(classify(Some("module"), None), ScriptKind::Module);
        assert_eq!(classify(Some("application/json"), None), ScriptKind::Data);
        assert_eq!(classify(Some("text/template"), None), ScriptKind::Data);
        assert_eq!(classify(None, Some("JavaScript")), ScriptKind::Classic);
        assert_eq!(classify(None, Some("vbscript")), ScriptKind::Data);
    }

    #[test]
    fn decodes_scripts_by_bom_then_charset() {
        assert_eq!(
            decode_text(b"var a='\xE9'", Some("text/javascript; charset=latin1")),
            "var a='é'"
        );
        assert_eq!(decode_text("x='é'".as_bytes(), None), "x='é'");
        assert_eq!(
            decode_text(b"\xEF\xBB\xBFx=1", Some("text/javascript; charset=latin1")),
            "x=1"
        );
    }
}
