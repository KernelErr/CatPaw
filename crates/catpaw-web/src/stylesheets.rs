//! The page's style sheets, and the styles computed from them.
//!
//! `<style>` elements contribute their text. `<link rel=stylesheet>`
//! elements are fetched when they come to refer to a sheet; they fire
//! `load` or `error`, delay the document's `load` event and, when the
//! parser inserted them, the scripts that follow. Styles are resolved on
//! demand, for the elements script asks about.
//!
//! Not there yet: `@import`, alternate style sheets, and the CSSOM view of
//! sheets (`sheet`, `document.styleSheets`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::{Duration, Instant};

use catpaw_dom::{Dom, NodeId};
use catpaw_js::EventTargetRef;
use catpaw_style::{MediaQueryList, Pseudo, StyleEngine};

use crate::element::child_text_content;
use crate::net::{self, NetRequest, NetResponse, NetResult, RequestKind};
use crate::page::{Cx, PageState};
use crate::{events, media, scripting};
use url::Url;

/// How long scripts wait for the style sheets ahead of them.
const SCRIPT_BLOCKING_LIMIT: Duration = Duration::from_secs(10);

enum LinkState {
    /// Being fetched, under this request token if there is a network.
    Loading(Option<u64>),
    Loaded(Rc<str>),
    Failed,
}

/// A `link` element that refers to a style sheet.
struct Link {
    url: Url,
    state: LinkState,
    /// Still loading, and holding up the scripts the parser comes across.
    blocks_scripts: bool,
}

/// The page's style state.
#[derive(Default)]
pub(crate) struct Styles {
    engine: RefCell<Option<StyleEngine>>,
    /// The DOM version the engine's sheets and resolved styles belong to.
    version: Cell<Option<u64>>,
    links: RefCell<HashMap<NodeId, Link>>,
    /// The number of loading sheets that hold up scripts.
    blocking: Cell<u32>,
}

fn media_applies(page: &PageState, media: Option<&str>) -> bool {
    match media.map(str::trim) {
        None | Some("") => true,
        Some(query) => MediaQueryList::parse(query).matches(&media::device(page)),
    }
}

/// The style sheet a `link` element refers to, if it does, and its `media`
/// attribute.
fn link_target(page: &PageState, dom: &Dom, el: NodeId) -> Option<(Url, Option<String>)> {
    let data = dom.element(el)?;
    if !data.is_html() || &*data.name.local != "link" || !dom.is_connected(el) {
        return None;
    }
    let rel = data.attr("rel")?;
    let has = |name: &str| {
        rel.split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case(name))
    };
    if !has("stylesheet") || has("alternate") || data.has_attr("disabled") {
        return None;
    }
    let href = data.attr("href")?.trim();
    if href.is_empty() {
        return None;
    }
    let url = page.resolve_url(href)?;
    Some((url, data.attr("media").map(str::to_string)))
}

/// Drops what is known about a `link` element's sheet, abandoning its fetch.
fn forget(page: &PageState, el: NodeId) {
    let Some(link) = page.styles.links.borrow_mut().remove(&el) else {
        return;
    };
    if let LinkState::Loading(token) = link.state {
        if let Some(token) = token {
            net::abort_request(page, token);
        }
        if link.blocks_scripts {
            let blocking = &page.styles.blocking;
            blocking.set(blocking.get().saturating_sub(1));
        }
        scripting::unblock_load(page);
    }
    page.styles.version.set(None);
}

/// Looks at an element again after it was inserted or one of its attributes
/// changed: if it is a `link` that now refers to a style sheet, the sheet is
/// fetched; one it no longer refers to is dropped.
pub(crate) fn link_changed(page: &PageState, el: NodeId, parser_inserted: bool) {
    let target = {
        let dom = page.dom.borrow();
        if !dom.contains(el) || !dom.is_html_element(el, "link") {
            return;
        }
        link_target(page, &dom, el)
    };
    {
        let links = page.styles.links.borrow();
        match (links.get(&el), &target) {
            (None, None) => return,
            (Some(link), Some((url, _))) if link.url == *url => return,
            _ => {}
        }
    }
    forget(page, el);
    let Some((url, media)) = target else {
        return;
    };

    let blocks_scripts = parser_inserted && media_applies(page, media.as_deref());
    let mut request = NetRequest::get(url.clone(), RequestKind::Style);
    request.referrer = Some(page.url.borrow().clone());
    request
        .headers
        .push(("Accept".to_string(), "text/css,*/*;q=0.1".to_string()));
    scripting::block_load(page);
    if blocks_scripts {
        page.styles.blocking.set(page.styles.blocking.get() + 1);
    }
    let requested = url.clone();
    let token = net::start_request(page, request, move |cx, outcome| {
        loaded(cx, el, &requested, outcome);
    });
    page.styles.links.borrow_mut().insert(
        el,
        Link {
            url,
            state: LinkState::Loading(token),
            blocks_scripts,
        },
    );
}

/// Called when `node` was removed from its parent: the sheets of the `link`
/// elements in it no longer apply.
pub(crate) fn subtree_removed(page: &PageState, node: NodeId) {
    if page.styles.links.borrow().is_empty() {
        return;
    }
    let gone: Vec<NodeId> = {
        let dom = page.dom.borrow();
        let links = page.styles.links.borrow();
        dom.traverse(node)
            .filter(|n| links.contains_key(n))
            .collect()
    };
    for el in gone {
        forget(page, el);
    }
}

/// The text of a fetched style sheet, if the response is one.
fn sheet_text(page: &PageState, response: &NetResponse) -> Option<String> {
    if !(200..300).contains(&response.status) {
        return None;
    }
    let content_type = response.header("content-type");
    let is_css = content_type.is_some_and(|value| {
        let essence = value.split(';').next().unwrap_or_default().trim();
        essence.eq_ignore_ascii_case("text/css")
    });
    // Quirks mode lets a sheet from the document's own origin be mislabelled.
    let lenient = page.dom.borrow().quirks_mode() == catpaw_dom::QuirksMode::Quirks
        && response.url.origin() == page.url.borrow().origin();
    if !is_css && !lenient {
        page.log(
            crate::ConsoleLevel::Warn,
            format!(
                "The style sheet {} was not applied: its content type is not text/css",
                response.url
            ),
        );
        return None;
    }
    Some(scripting::decode_text(&response.body, content_type))
}

fn loaded(cx: &mut Cx<'_>, el: NodeId, url: &Url, outcome: NetResult) {
    let page = cx.page;
    let text = outcome
        .ok()
        .and_then(|response| sheet_text(page, &response));
    let ok = text.is_some();
    {
        let mut links = page.styles.links.borrow_mut();
        // The element may have moved on to another sheet, or to none.
        let Some(link) = links.get_mut(&el).filter(|link| link.url == *url) else {
            return;
        };
        if !matches!(link.state, LinkState::Loading(_)) {
            return;
        }
        link.state = match text {
            Some(text) => LinkState::Loaded(text.into()),
            None => LinkState::Failed,
        };
        if std::mem::take(&mut link.blocks_scripts) {
            let blocking = &page.styles.blocking;
            blocking.set(blocking.get().saturating_sub(1));
        }
    }
    page.styles.version.set(None);
    if cx.dom().contains(el) {
        let type_ = if ok { "load" } else { "error" };
        events::fire(cx, EventTargetRef::Node(el), type_, false, false);
    }
    scripting::unblock_load(page);
}

/// Waits for the style sheets that hold up scripts: those the parser has
/// come across that are still loading. Requests that complete meanwhile are
/// delivered.
pub(crate) fn wait_for_blocking_sheets(cx: &mut Cx<'_>) {
    let styles = &cx.page.styles;
    if styles.blocking.get() == 0 {
        return;
    }
    let deadline = Instant::now() + SCRIPT_BLOCKING_LIMIT;
    // Without requests in flight nothing can arrive (with no network at
    // all, failures are delivered as tasks).
    while styles.blocking.get() > 0 && net::inflight(cx.page) > 0 {
        if Instant::now() >= deadline {
            // Scripts stop waiting for sheets this slow.
            for link in styles.links.borrow_mut().values_mut() {
                link.blocks_scripts = false;
            }
            styles.blocking.set(0);
            break;
        }
        net::deliver(cx, Some(Duration::from_millis(20)));
    }
}

/// Identifies a sheet by where it comes from and what it says.
fn sheet_key(owner: NodeId, text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    owner.hash(&mut hasher);
    text.hash(&mut hasher);
    hasher.finish()
}

/// The document's style sheets in tree order: each owner with its text.
fn collect_sheets(page: &PageState, dom: &Dom) -> Vec<(NodeId, Rc<str>)> {
    let links = page.styles.links.borrow();
    let mut sheets = Vec::new();
    for node in dom.descendants(dom.document()) {
        let Some(el) = dom.element(node) else {
            continue;
        };
        if !el.is_html() {
            continue;
        }
        let text: Rc<str> = match &*el.name.local {
            "style" => {
                let is_css = el.attr("type").is_none_or(|t| {
                    let t = t.trim();
                    t.is_empty() || t.eq_ignore_ascii_case("text/css")
                });
                if !is_css {
                    continue;
                }
                child_text_content(dom, node).into()
            }
            "link" => match links.get(&node) {
                Some(Link {
                    state: LinkState::Loaded(text),
                    ..
                }) => text.clone(),
                _ => continue,
            },
            _ => continue,
        };
        if media_applies(page, el.attr("media")) {
            sheets.push((node, text));
        }
    }
    sheets
}

/// Brings the style engine up to date with the document and runs `f` on it.
fn with_engine<R>(page: &PageState, f: impl FnOnce(&mut StyleEngine, &Dom) -> R) -> R {
    let dom = page.dom.borrow();
    let mut engine = page.styles.engine.borrow_mut();
    let engine = engine.get_or_insert_with(|| StyleEngine::new(&media::device(page)));
    let version = dom.version();
    if page.styles.version.get() != Some(version) {
        engine.set_quirks_mode(dom.quirks_mode());
        let sheets = collect_sheets(page, &dom);
        let keyed: Vec<(u64, &str)> = sheets
            .iter()
            .map(|(owner, text)| (sheet_key(*owner, text), &**text))
            .collect();
        engine.set_author_stylesheets(&keyed);
        engine.invalidate();
        page.styles.version.set(Some(version));
    }
    f(engine, &dom)
}

/// The computed value of `property` for `element` or one of its
/// pseudo-elements. Empty when the element is not in the document.
pub(crate) fn computed_value(
    page: &PageState,
    element: NodeId,
    pseudo: Option<Pseudo>,
    property: &str,
) -> String {
    with_engine(page, |engine, dom| {
        engine
            .computed_style(dom, element, pseudo)
            .map(|style| style.get(property))
            .unwrap_or_default()
    })
}
