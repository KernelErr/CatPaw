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

use catpaw_dom::{HtmlParseOptions, HtmlStream, NodeId, WriteQueue};
use catpaw_js::EventTargetRef;
use encoding_rs::Encoding;
use url::Url;

use crate::element::child_text_content;
use crate::event_loop::queue_task;
use crate::events;
use crate::generated::DocumentReadyState;
use crate::net::{self, NetRequest, NetResult, RequestKind};
use crate::page::{ConsoleLevel, Cx};

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
    warned_about_modules: Cell<bool>,
}

impl ScriptState {
    /// Whether the document's `load` event is still to come.
    pub fn is_loading(&self) -> bool {
        self.parser.borrow().is_some() || self.load_pending.get()
    }
}

struct Deferred {
    element: NodeId,
    url: Url,
    result: Rc<RefCell<Option<NetResult>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScriptKind {
    Classic,
    Module,
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
    } else {
        ScriptKind::Data
    }
}

/// Decodes a fetched script: the BOM wins, then the Content-Type charset,
/// then UTF-8.
fn decode_script(body: &[u8], content_type: Option<&str>) -> String {
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
fn execute(cx: &mut Cx<'_>, el: NodeId, source: &str, url: &str, parser_inserted: bool) {
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
    let result = cx.script.eval_script(source, url, 1);
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

/// Runs the body of a fetched script, or fires `error` if the fetch failed.
fn execute_fetched(
    cx: &mut Cx<'_>,
    el: NodeId,
    url: &Url,
    result: NetResult,
    parser_inserted: bool,
) {
    match result {
        Ok(response) if response.is_success() => {
            let source = decode_script(&response.body, response.header("content-type"));
            execute(cx, el, &source, response.url.as_str(), parser_inserted);
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

fn block_load(cx: &Cx<'_>) {
    let scripts = &cx.page.scripts;
    scripts.load_blockers.set(scripts.load_blockers.get() + 1);
}

fn unblock_load(cx: &mut Cx<'_>) {
    let scripts = &cx.page.scripts;
    scripts
        .load_blockers
        .set(scripts.load_blockers.get().saturating_sub(1));
    maybe_fire_load(cx);
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
        if src.is_none() && child_text_content(&dom, el).is_empty() {
            return;
        }
        if !dom.is_connected(el) {
            return;
        }
        if kind == ScriptKind::Data {
            return;
        }
        if let Some(data) = dom.element_mut(el) {
            data.script_already_started = true;
        }
        (src, kind, flags.0, flags.1)
    };

    if kind == ScriptKind::Module {
        if !cx.page.scripts.warned_about_modules.replace(true) {
            cx.page.log(
                ConsoleLevel::Warn,
                "Module scripts are not supported yet and were skipped",
            );
        }
        return;
    }

    let Some(src) = src else {
        let source = child_text_content(&cx.dom(), el);
        let url = cx.page.url.borrow().to_string();
        execute(cx, el, &source, &url, parser_inserted);
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
    request.referrer = Some(cx.page.url.borrow().clone());

    if parser_inserted && !is_async && !is_defer {
        // Parser-blocking: nothing else happens until it has run.
        let result = match cx.page.net() {
            Some(net) => net.fetch_blocking(request),
            None => Err("no network available".to_string()),
        };
        execute_fetched(cx, el, &url, result, true);
        return;
    }

    if parser_inserted && is_defer && !is_async {
        // Fetched while parsing continues; run in order when it ends.
        let result = Rc::new(RefCell::new(None));
        let slot = result.clone();
        let started = net::start_request(cx.page, request, move |_cx, outcome| {
            *slot.borrow_mut() = Some(outcome);
        });
        if started.is_none() {
            *result.borrow_mut() = Some(Err("no network available".to_string()));
        }
        cx.page.scripts.deferred.borrow_mut().push(Deferred {
            element: el,
            url,
            result,
        });
        return;
    }

    // Async (and every script-inserted external script): run when fetched.
    block_load(cx);
    let callback_url = url.clone();
    let started = net::start_request(cx.page, request, move |cx, outcome| {
        execute_fetched(cx, el, &callback_url, outcome, false);
        unblock_load(cx);
    });
    if started.is_none() {
        queue_task(cx.page, "script error", move |cx| {
            execute_fetched(cx, el, &url, Err("no network available".to_string()), false);
            unblock_load(cx);
        });
    }
}

fn run_parser_script(cx: &mut Cx<'_>, el: NodeId) {
    // Microtasks queued by earlier scripts run before the next one starts.
    cx.checkpoint();
    prepare(cx, el, true);
}

fn pump(cx: &mut Cx<'_>, stream: &HtmlStream) {
    while let Some(script) = stream.pump() {
        run_parser_script(cx, script);
        if cx.page.navigation.borrow().is_some() {
            return;
        }
    }
}

fn set_ready_state(cx: &mut Cx<'_>, state: DocumentReadyState) {
    cx.page.document_state.borrow_mut().ready_state = state;
    let document = cx.document();
    events::fire(
        cx,
        EventTargetRef::Node(document),
        "readystatechange",
        false,
        false,
    );
}

fn maybe_fire_load(cx: &mut Cx<'_>) {
    let scripts = &cx.page.scripts;
    if !scripts.load_pending.get() || scripts.load_blockers.get() > 0 {
        return;
    }
    scripts.load_pending.set(false);
    queue_task(cx.page, "load", |cx| {
        set_ready_state(cx, DocumentReadyState::Complete);
        events::fire(cx, EventTargetRef::Window, "load", false, false);
        events::fire(cx, EventTargetRef::Window, "pageshow", false, false);
    });
}

/// <https://html.spec.whatwg.org/multipage/#the-end>
fn finish_parsing(cx: &mut Cx<'_>) {
    set_ready_state(cx, DocumentReadyState::Interactive);

    let deferred = std::mem::take(&mut *cx.page.scripts.deferred.borrow_mut());
    for script in deferred {
        // Wait for this script's fetch; other fetches complete meanwhile.
        while script.result.borrow().is_none() && net::inflight(cx.page) > 0 {
            net::deliver(cx, Some(Duration::from_millis(50)));
        }
        let result = script
            .result
            .borrow_mut()
            .take()
            .unwrap_or_else(|| Err("the request was abandoned".to_string()));
        cx.checkpoint();
        execute_fetched(cx, script.element, &script.url, result, false);
        if cx.page.navigation.borrow().is_some() {
            return;
        }
    }

    let document = cx.document();
    events::fire(
        cx,
        EventTargetRef::Node(document),
        "DOMContentLoaded",
        true,
        false,
    );
    cx.page.scripts.load_pending.set(true);
    maybe_fire_load(cx);
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
    stream.push(html);
    pump(cx, &stream);
    stream.finish();
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
    {
        let mut dom = cx.dom_mut();
        let document = dom.document();
        let children: Vec<NodeId> = dom.children(document).collect();
        for child in children {
            dom.detach(child);
        }
    }
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
        stream.finish();
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
    while let Some(script) = stream.pump_write(&queue) {
        run_parser_script(cx, script);
    }
}

// ---- tree-mutation hooks ---------------------------------------------------

/// Called after script inserted `inserted` under `parent`.
pub(crate) fn nodes_inserted(cx: &mut Cx<'_>, parent: NodeId, inserted: &[NodeId]) {
    let scripts: Vec<NodeId> = {
        let dom = cx.dom();
        if !dom.is_connected(parent) {
            return;
        }
        let mut found = Vec::new();
        // Content added to a script element that has not run yet.
        if dom.is_html_element(parent, "script") {
            found.push(parent);
        }
        for &node in inserted {
            found.extend(
                dom.traverse(node)
                    .filter(|&n| dom.is_html_element(n, "script")),
            );
        }
        found
    };
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
            decode_script(b"var a='\xE9'", Some("text/javascript; charset=latin1")),
            "var a='é'"
        );
        assert_eq!(decode_script("x='é'".as_bytes(), None), "x='é'");
        assert_eq!(
            decode_script(b"\xEF\xBB\xBFx=1", Some("text/javascript; charset=latin1")),
            "x=1"
        );
    }
}
