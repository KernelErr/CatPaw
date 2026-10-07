//! The `Document` interface.
//!
//! A page has one document with a window: the arena's own. Script can make
//! others (`DOMParser`, `document.implementation`); they have a tree and
//! nothing else, and answer accordingly.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use catpaw_dom::{
    DocumentData, Dom, FragmentKind, LocalName, Namespace, NodeId, NodeKind, QualName, QuirksMode,
};
use catpaw_js::{Callback, Exception, Fallible, ObjectId, Value, WindowRef};

use crate::collections::{self, DocumentKind, ListSource};
use crate::element::{child_text_content, is_valid_element_name, validate_and_extract};
use crate::generated::{self as web, DocumentReadyState, DocumentVisibilityState, InterfaceId};
use crate::page::Cx;
use crate::{Web, events, implementation, node, scripting, window};

const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

fn html_element(dom: &Dom, document: NodeId) -> Option<NodeId> {
    dom.child_elements(document)
        .next()
        .filter(|&e| dom.is_html_element(e, "html"))
}

pub(crate) fn head(dom: &Dom, document: NodeId) -> Option<NodeId> {
    let html = html_element(dom, document)?;
    dom.child_elements(html)
        .find(|&c| dom.is_html_element(c, "head"))
}

pub(crate) fn body(dom: &Dom, document: NodeId) -> Option<NodeId> {
    let html = html_element(dom, document)?;
    dom.child_elements(html)
        .find(|&c| dom.is_html_element(c, "body") || dom.is_html_element(c, "frameset"))
}

fn title_element(dom: &Dom, document: NodeId) -> Option<NodeId> {
    dom.descendants(document)
        .find(|&n| dom.is_html_element(n, "title"))
}

/// Whether `document` is an HTML document rather than an XML document.
pub(crate) fn is_html_document(dom: &Dom, document: NodeId) -> bool {
    dom.document_data_of(document).is_none_or(|d| !d.is_xml)
}

/// Whether `document` is the page's own: the one with a window.
fn has_window(cx: &Cx<'_>, document: NodeId) -> bool {
    document == cx.document()
}

/// Hands `node`, just created, to the document that created it.
fn created(cx: &Cx<'_>, document: NodeId, node: NodeId) -> NodeId {
    cx.dom_mut().adopt_subtree(node, document);
    node
}

// ---- named properties and collections -------------------------------------

/// Whether `node` is an element that can give the document a named
/// property: `embed`, `form`, `iframe`, `img` or `object`.
pub(crate) fn is_nameable(dom: &Dom, node: NodeId) -> bool {
    dom.element(node).is_some_and(|el| {
        el.is_html()
            && matches!(
                &*el.name.local,
                "embed" | "form" | "iframe" | "img" | "object"
            )
    })
}

/// Whether the subtree at `node` holds a nameable element.
pub(crate) fn has_nameable(dom: &Dom, node: NodeId) -> bool {
    dom.traverse(node).any(|n| is_nameable(dom, n))
}

/// An `embed` or `object` is exposed unless an `object` contains it; an
/// `object` must also contain neither. (Fallback content is never shown
/// here, so that is the whole rule.)
fn exposed(dom: &Dom, node: NodeId) -> bool {
    if dom
        .ancestors(node)
        .any(|a| dom.is_html_element(a, "object"))
    {
        return false;
    }
    !dom.is_html_element(node, "object")
        || !dom
            .descendants(node)
            .any(|d| dom.is_html_element(d, "object") || dom.is_html_element(d, "embed"))
}

/// The names `node` gives the document, an id before a name
/// (<https://html.spec.whatwg.org/multipage/dom.html#dom-document-nameditem>).
fn contributed_names(dom: &Dom, node: NodeId) -> Vec<String> {
    let Some(el) = dom.element(node).filter(|el| el.is_html()) else {
        return Vec::new();
    };
    let name = el.attr("name").filter(|n| !n.is_empty());
    let id = el.attr("id").filter(|i| !i.is_empty());
    let mut names: Vec<&str> = Vec::new();
    match &*el.name.local {
        "form" | "iframe" => names.extend(name),
        "img" => {
            if name.is_some() {
                names.extend(id);
            }
            names.extend(name);
        }
        "embed" if exposed(dom, node) => names.extend(name),
        "object" if exposed(dom, node) => {
            names.extend(id);
            names.extend(name);
        }
        _ => {}
    }
    names.dedup();
    names.into_iter().map(str::to_string).collect()
}

/// The elements `name` refers to, in tree order.
pub(crate) fn named_elements(dom: &Dom, document: NodeId, name: &str) -> Vec<NodeId> {
    if name.is_empty() || !dom.contains(document) {
        return Vec::new();
    }
    dom.descendants(document)
        .filter(|&n| is_nameable(dom, n) && contributed_names(dom, n).iter().any(|c| c == name))
        .collect()
}

/// The document's supported property names, in tree order.
fn supported_names(dom: &Dom, document: NodeId) -> Vec<String> {
    let mut names = Vec::new();
    if !dom.contains(document) {
        return names;
    }
    for node in dom.descendants(document) {
        if is_nameable(dom, node) {
            for name in contributed_names(dom, node) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// The page document's named properties, by name. `document.x` is looked
/// up before the prototype chain on every property access, so the map is
/// rebuilt only when a nameable element came, went or was renamed (the
/// tree hooks call [`DocumentNames::changed`]), not on every tree change.
#[derive(Default)]
pub(crate) struct DocumentNames {
    version: Cell<u64>,
    built_for: Cell<Option<u64>>,
    names: RefCell<HashMap<String, Vec<NodeId>>>,
}

impl DocumentNames {
    /// Notes that the names may have changed.
    pub(crate) fn changed(&self) {
        self.version.set(self.version.get() + 1);
    }

    fn lookup(&self, dom: &Dom, document: NodeId, name: &str) -> Vec<NodeId> {
        if self.built_for.get() != Some(self.version.get()) {
            let mut names: HashMap<String, Vec<NodeId>> = HashMap::new();
            for node in dom.descendants(document) {
                if is_nameable(dom, node) {
                    for n in contributed_names(dom, node) {
                        names.entry(n).or_default().push(node);
                    }
                }
            }
            *self.names.borrow_mut() = names;
            self.built_for.set(Some(self.version.get()));
        }
        self.names.borrow().get(name).cloned().unwrap_or_default()
    }
}

/// `document.embeds`: one object for the page's document, so that
/// `plugins` can be the same one.
fn embeds(cx: &mut Cx<'_>, document: NodeId) -> ObjectId {
    if !has_window(cx, document) {
        return kind_collection(cx, document, DocumentKind::Embeds);
    }
    window::singleton(
        cx,
        |s| &mut s.embeds,
        |page| {
            collections::html_collection(
                page,
                ListSource::DocumentKind {
                    root: document,
                    kind: DocumentKind::Embeds,
                },
            )
        },
    )
}

fn kind_collection(cx: &Cx<'_>, document: NodeId, kind: DocumentKind) -> ObjectId {
    collections::html_collection(
        cx.page,
        ListSource::DocumentKind {
            root: document,
            kind,
        },
    )
}

fn url(cx: &Cx<'_>, document: NodeId) -> String {
    if has_window(cx, document) {
        return cx.page.url.borrow().to_string();
    }
    let dom = cx.dom();
    let url = dom.document_data_of(document).and_then(|d| d.url.as_ref());
    url.map_or_else(|| "about:blank".to_string(), ToString::to_string)
}

fn charset(cx: &Cx<'_>, document: NodeId) -> String {
    if has_window(cx, document) {
        cx.page.document_state.borrow().charset.clone()
    } else {
        "UTF-8".to_string()
    }
}

impl web::DocumentImpl for Web {
    /// `new Document()`: an XML document, with the page's URL.
    fn constructor(cx: &mut Cx<'_>) -> Fallible<NodeId> {
        let url = cx.page.url.borrow().clone();
        Ok(cx.dom_mut().create_document(DocumentData {
            quirks_mode: QuirksMode::NoQuirks,
            url: Some(url),
            is_xml: true,
            content_type: Some("application/xml".to_string()),
        }))
    }

    fn url(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        Ok(url(cx, this))
    }

    fn document_uri(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        Ok(url(cx, this))
    }

    fn compat_mode(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        let dom = cx.dom();
        let quirks = dom
            .document_data_of(this)
            .is_some_and(|d| d.quirks_mode == QuirksMode::Quirks);
        Ok(if quirks { "BackCompat" } else { "CSS1Compat" }.to_string())
    }

    fn character_set(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        Ok(charset(cx, this))
    }

    fn charset(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        Ok(charset(cx, this))
    }

    fn input_encoding(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        Ok(charset(cx, this))
    }

    fn content_type(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        if has_window(cx, this) {
            return Ok(cx.page.document_state.borrow().content_type.clone());
        }
        let dom = cx.dom();
        let data = dom.document_data_of(this);
        Ok(match data.and_then(|d| d.content_type.clone()) {
            Some(content_type) => content_type,
            None if data.is_some_and(|d| d.is_xml) => "application/xml".to_string(),
            None => "text/html".to_string(),
        })
    }

    fn doctype(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        let dom = cx.dom();
        Ok(dom
            .children(this)
            .find(|&c| matches!(dom.kind(c), NodeKind::Doctype(_))))
    }

    fn document_element(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(cx.dom().child_elements(this).next())
    }

    /// The document's selection, or none for a document without a window.
    fn get_selection(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<ObjectId>> {
        if this != cx.page.document() {
            return Ok(None);
        }
        Ok(Some(crate::selection::selection(cx)))
    }

    fn create_range(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(crate::range::create_range(cx, this))
    }

    fn images(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(kind_collection(cx, this, DocumentKind::Images))
    }

    fn embeds(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(embeds(cx, this))
    }

    /// The same collection as `embeds`.
    fn plugins(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(embeds(cx, this))
    }

    fn links(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(kind_collection(cx, this, DocumentKind::Links))
    }

    fn forms(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(kind_collection(cx, this, DocumentKind::Forms))
    }

    fn scripts(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(kind_collection(cx, this, DocumentKind::Scripts))
    }

    fn anchors(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(kind_collection(cx, this, DocumentKind::Anchors))
    }

    fn applets(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(kind_collection(cx, this, DocumentKind::Applets))
    }

    /// <https://html.spec.whatwg.org/multipage/dom.html#dom-document-nameditem>
    fn named_get(cx: &mut Cx<'_>, this: NodeId, name: &str) -> Fallible<Option<Value>> {
        let nodes = if has_window(cx, this) {
            cx.page.document_names.lookup(&cx.dom(), this, name)
        } else {
            named_elements(&cx.dom(), this, name)
        };
        Ok(match nodes.as_slice() {
            [] => None,
            // An iframe would be its window; there are no frames yet.
            [one] => Some(Value::Node(*one)),
            _ => Some(Value::Object(collections::html_collection(
                cx.page,
                ListSource::DocumentNamed {
                    root: this,
                    name: name.to_string(),
                },
            ))),
        })
    }

    fn named_properties(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Vec<String>> {
        Ok(supported_names(&cx.dom(), this))
    }

    fn element_from_point(
        cx: &mut Cx<'_>,
        this: NodeId,
        x: f64,
        y: f64,
    ) -> Fallible<Option<NodeId>> {
        if this != cx.document() || !x.is_finite() || !y.is_finite() {
            return Ok(None);
        }
        Ok(crate::layout::element_from_point(
            cx.page, x as f32, y as f32,
        ))
    }

    fn elements_from_point(cx: &mut Cx<'_>, this: NodeId, x: f64, y: f64) -> Fallible<Vec<NodeId>> {
        if this != cx.document() || !x.is_finite() || !y.is_finite() {
            return Ok(Vec::new());
        }
        Ok(crate::layout::elements_from_point(
            cx.page, x as f32, y as f32,
        ))
    }

    /// <https://drafts.csswg.org/cssom-view/#dom-document-scrollingelement>
    fn scrolling_element(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        let dom = cx.dom();
        let quirks = dom
            .document_data_of(this)
            .is_some_and(|d| d.quirks_mode == QuirksMode::Quirks);
        // Without layout no body is potentially scrollable, so quirks mode
        // answers with the body itself, as a browser would for a static page.
        Ok(if quirks {
            body(&dom, this)
        } else {
            dom.child_elements(this).next()
        })
    }

    fn get_elements_by_tag_name(
        cx: &mut Cx<'_>,
        this: NodeId,
        qualified_name: String,
    ) -> Fallible<ObjectId> {
        Ok(collections::html_collection(
            cx.page,
            ListSource::TagName {
                root: this,
                name: qualified_name,
            },
        ))
    }

    fn get_elements_by_tag_name_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        local_name: String,
    ) -> Fallible<ObjectId> {
        Ok(collections::html_collection(
            cx.page,
            ListSource::TagNameNS {
                root: this,
                namespace: namespace.unwrap_or_default(),
                local: local_name,
            },
        ))
    }

    fn get_elements_by_class_name(
        cx: &mut Cx<'_>,
        this: NodeId,
        class_names: String,
    ) -> Fallible<ObjectId> {
        Ok(collections::html_collection(
            cx.page,
            ListSource::ClassNames {
                root: this,
                classes: class_names
                    .split_ascii_whitespace()
                    .map(str::to_string)
                    .collect(),
            },
        ))
    }

    fn create_element(
        cx: &mut Cx<'_>,
        this: NodeId,
        local_name: String,
        options: web::StringOrElementCreationOptions,
    ) -> Fallible<NodeId> {
        if !is_valid_element_name(&local_name) {
            return Err(Exception::invalid_character(format!(
                "'{local_name}' is not a valid element name"
            )));
        }
        // An HTML document makes HTML elements, with lowercase names.
        let (html, xhtml) = {
            let dom = cx.dom();
            let data = dom.document_data_of(this);
            let xhtml =
                data.and_then(|d| d.content_type.as_deref()) == Some("application/xhtml+xml");
            (is_html_document(&dom, this), xhtml)
        };
        let local = if html {
            local_name.to_ascii_lowercase()
        } else {
            local_name
        };
        let is = match options {
            web::StringOrElementCreationOptions::ElementCreationOptions(o) => o.is,
            web::StringOrElementCreationOptions::String(_) => None,
        };
        if html
            && let Some(element) =
                crate::custom_elements::create_synchronously(cx, this, &local, is.as_deref())
        {
            return Ok(element);
        }
        let namespace = if html || xhtml { HTML_NS } else { "" };
        let name = QualName::new(None, Namespace::from(namespace), LocalName::from(local));
        let element = {
            let mut dom = cx.dom_mut();
            let element = node::create_element_node(&mut dom, name);
            if let (Some(is), Some(data)) = (is, dom.element_mut(element)) {
                data.is_value = Some(is);
            }
            element
        };
        Ok(created(cx, this, element))
    }

    fn create_element_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        qualified_name: String,
        _options: web::StringOrElementCreationOptions,
    ) -> Fallible<NodeId> {
        let name = validate_and_extract(namespace, &qualified_name, true)?;
        let element = node::create_element_node(&mut cx.dom_mut(), name);
        Ok(created(cx, this, element))
    }

    fn create_document_fragment(cx: &mut Cx<'_>, this: NodeId) -> Fallible<NodeId> {
        let fragment = cx.dom_mut().create_fragment(FragmentKind::Plain);
        Ok(created(cx, this, fragment))
    }

    fn create_text_node(cx: &mut Cx<'_>, this: NodeId, data: String) -> Fallible<NodeId> {
        let text = cx.dom_mut().create_text(data);
        Ok(created(cx, this, text))
    }

    fn create_comment(cx: &mut Cx<'_>, this: NodeId, data: String) -> Fallible<NodeId> {
        let comment = cx.dom_mut().create_comment(data);
        Ok(created(cx, this, comment))
    }

    /// <https://dom.spec.whatwg.org/#dom-document-createprocessinginstruction>
    fn create_processing_instruction(
        cx: &mut Cx<'_>,
        this: NodeId,
        target: String,
        data: String,
    ) -> Fallible<NodeId> {
        if !is_valid_element_name(&target) {
            return Err(Exception::invalid_character(format!(
                "'{target}' is not a valid processing instruction target"
            )));
        }
        if data.contains("?>") {
            return Err(Exception::invalid_character(
                "The data must not contain '?>'",
            ));
        }
        let pi = cx.dom_mut().create_processing_instruction(target, data);
        Ok(created(cx, this, pi))
    }

    fn import_node(
        cx: &mut Cx<'_>,
        this: NodeId,
        node: NodeId,
        options: web::BooleanOrImportNodeOptions,
    ) -> Fallible<NodeId> {
        node::check(cx, node)?;
        if matches!(cx.dom().kind(node), NodeKind::Document(_)) {
            return Err(Exception::not_supported("A document cannot be imported"));
        }
        let deep = match options {
            web::BooleanOrImportNodeOptions::Boolean(deep) => deep,
            web::BooleanOrImportNodeOptions::ImportNodeOptions(o) => !o.self_only,
        };
        let copy = node::clone_node(cx, node, deep)?;
        Ok(created(cx, this, copy))
    }

    fn adopt_node(cx: &mut Cx<'_>, this: NodeId, node: NodeId) -> Fallible<NodeId> {
        node::check(cx, node)?;
        if matches!(cx.dom().kind(node), NodeKind::Document(_)) {
            return Err(Exception::not_supported("A document cannot be adopted"));
        }
        node::remove(cx, node, false);
        Ok(created(cx, this, node))
    }

    fn create_attribute(cx: &mut Cx<'_>, this: NodeId, local_name: String) -> Fallible<ObjectId> {
        crate::attributes::create(cx, this, local_name)
    }

    fn create_attribute_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        qualified_name: String,
    ) -> Fallible<ObjectId> {
        crate::attributes::create_ns(cx, this, namespace, &qualified_name)
    }

    fn create_node_iterator(
        cx: &mut Cx<'_>,
        _this: NodeId,
        root: NodeId,
        what_to_show: u32,
        filter: Option<Callback>,
    ) -> Fallible<ObjectId> {
        crate::traversal::node_iterator(cx, root, what_to_show, filter)
    }

    fn create_tree_walker(
        cx: &mut Cx<'_>,
        _this: NodeId,
        root: NodeId,
        what_to_show: u32,
        filter: Option<Callback>,
    ) -> Fallible<ObjectId> {
        crate::traversal::tree_walker(cx, root, what_to_show, filter)
    }

    fn create_event(cx: &mut Cx<'_>, _this: NodeId, interface: String) -> Fallible<ObjectId> {
        let iface = match interface.to_ascii_lowercase().as_str() {
            "event" | "events" | "htmlevents" | "svgevents" => InterfaceId::Event,
            "customevent" => InterfaceId::CustomEvent,
            "hashchangeevent" => InterfaceId::HashChangeEvent,
            _ => {
                return Err(Exception::not_supported(format!(
                    "The event interface '{interface}' is not supported"
                )));
            }
        };
        Ok(events::uninitialized_event(cx.page, iface))
    }

    fn implementation(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(implementation::of(cx, this))
    }

    fn location(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<ObjectId>> {
        Ok(has_window(cx, this).then(|| window::location(cx)))
    }

    fn domain(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        if !has_window(cx, this) {
            return Ok(String::new());
        }
        Ok(cx
            .page
            .url
            .borrow()
            .host_str()
            .unwrap_or_default()
            .to_string())
    }

    fn set_domain(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        // Relaxing the same-origin policy is not on offer; the one value
        // that changes nothing is let through.
        let current = <Web as web::DocumentImpl>::domain(cx, this)?;
        if value == current && !value.is_empty() {
            return Ok(());
        }
        Err(Exception::security("document.domain cannot be changed"))
    }

    fn referrer(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        if !has_window(cx, this) {
            return Ok(String::new());
        }
        Ok(cx.page.document_state.borrow().referrer.clone())
    }

    fn cookie(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        // Only the page's document has cookies.
        if !has_window(cx, this) {
            return Ok(String::new());
        }
        if let Some(net) = cx.page.net() {
            return Ok(net.cookies_for(&cx.page.url.borrow()));
        }
        let state = cx.page.document_state.borrow();
        Ok(state
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; "))
    }

    fn set_cookie(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        if !has_window(cx, this) {
            return Ok(());
        }
        if let Some(net) = cx.page.net() {
            net.set_cookie(&cx.page.url.borrow(), &value);
            return Ok(());
        }
        // Without a network there is no jar: keep name=value pairs so that
        // scripts can read back what they wrote.
        let pair = value.split(';').next().unwrap_or_default().trim();
        let (name, val) = pair.split_once('=').unwrap_or(("", pair));
        if name.is_empty() && val.is_empty() {
            return Ok(());
        }
        cx.page
            .document_state
            .borrow_mut()
            .cookies
            .insert(name.trim().to_string(), val.trim().to_string());
        Ok(())
    }

    fn ready_state(cx: &mut Cx<'_>, this: NodeId) -> Fallible<DocumentReadyState> {
        if !has_window(cx, this) {
            return Ok(DocumentReadyState::Complete);
        }
        Ok(cx.page.document_state.borrow().ready_state)
    }

    fn title(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        let dom = cx.dom();
        let text = title_element(&dom, this)
            .map(|t| child_text_content(&dom, t))
            .unwrap_or_default();
        Ok(text.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
    }

    fn set_title(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        let existing = title_element(&cx.dom(), this);
        let title = match existing {
            Some(t) => t,
            None => {
                let Some(head) = head(&cx.dom(), this) else {
                    return Ok(());
                };
                let t = cx.dom_mut().create_html_element("title", Vec::new());
                node::append(cx, t, head)?;
                t
            }
        };
        node::string_replace_all(cx, &value, title);
        Ok(())
    }

    fn body(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(body(&cx.dom(), this))
    }

    fn set_body(cx: &mut Cx<'_>, this: NodeId, value: Option<NodeId>) -> Fallible<()> {
        let (current, root) = {
            let dom = cx.dom();
            let valid = value.is_some_and(|v| {
                dom.is_html_element(v, "body") || dom.is_html_element(v, "frameset")
            });
            if !valid {
                return Err(Exception::hierarchy_request(
                    "The new body must be a body or frameset element",
                ));
            }
            (body(&dom, this), dom.child_elements(this).next())
        };
        let Some(value) = value else { return Ok(()) };
        if current == Some(value) {
            return Ok(());
        }
        match (current, root) {
            (Some(old), _) => {
                let parent = cx.dom().parent(old);
                match parent {
                    Some(parent) => node::replace(cx, old, value, parent).map(drop),
                    None => Ok(()),
                }
            }
            (None, Some(root)) => node::append(cx, value, root).map(drop),
            (None, None) => Err(Exception::hierarchy_request(
                "The document has no root element",
            )),
        }
    }

    fn head(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(head(&cx.dom(), this))
    }

    fn get_elements_by_name(
        cx: &mut Cx<'_>,
        this: NodeId,
        element_name: String,
    ) -> Fallible<ObjectId> {
        Ok(collections::node_list(
            cx.page,
            ListSource::Name {
                root: this,
                name: element_name,
            },
        ))
    }

    fn current_script(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        if !has_window(cx, this) {
            return Ok(None);
        }
        Ok(cx.page.document_state.borrow().current_script)
    }

    fn open(
        cx: &mut Cx<'_>,
        this: NodeId,
        _unused1: Option<String>,
        _unused2: Option<String>,
    ) -> Fallible<NodeId> {
        if writable(cx, this)? {
            scripting::document_open(cx);
        }
        Ok(this)
    }

    fn open_overload2(
        _cx: &mut Cx<'_>,
        _this: NodeId,
        _url: String,
        _name: String,
        _features: String,
    ) -> Fallible<Option<WindowRef>> {
        // The three-argument form is window.open, which never opens a
        // window here (as with a popup blocker).
        Ok(None)
    }

    fn close(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        if writable(cx, this)? {
            scripting::document_close(cx);
        }
        Ok(())
    }

    fn write(cx: &mut Cx<'_>, this: NodeId, text: Vec<String>) -> Fallible<()> {
        if writable(cx, this)? {
            scripting::document_write(cx, &text.concat());
        }
        Ok(())
    }

    fn writeln(cx: &mut Cx<'_>, this: NodeId, text: Vec<String>) -> Fallible<()> {
        if writable(cx, this)? {
            let mut text = text.concat();
            text.push('\n');
            scripting::document_write(cx, &text);
        }
        Ok(())
    }

    fn default_view(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<WindowRef>> {
        Ok(has_window(cx, this).then_some(WindowRef::Local))
    }

    fn has_focus(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        Ok(has_window(cx, this))
    }

    fn hidden(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        Ok(!has_window(cx, this))
    }

    fn visibility_state(cx: &mut Cx<'_>, this: NodeId) -> Fallible<DocumentVisibilityState> {
        Ok(if has_window(cx, this) {
            DocumentVisibilityState::Visible
        } else {
            DocumentVisibilityState::Hidden
        })
    }
}

/// Whether `document.write()` and its companions act on `document`: only
/// the page's document has a parser to write to. An XML document refuses.
fn writable(cx: &Cx<'_>, document: NodeId) -> Fallible<bool> {
    if !is_html_document(&cx.dom(), document) {
        return Err(Exception::invalid_state(
            "An XML document cannot be written to",
        ));
    }
    Ok(has_window(cx, document))
}

impl web::DocumentOrShadowRootImpl for Web {
    fn style_sheets(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(crate::cssom::style_sheets(cx, this))
    }

    fn adopted_style_sheets(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Vec<ObjectId>> {
        Ok(crate::cssom::adopted_style_sheets(cx, this))
    }

    fn set_adopted_style_sheets(
        cx: &mut Cx<'_>,
        this: NodeId,
        value: Vec<ObjectId>,
    ) -> Fallible<()> {
        crate::cssom::set_adopted_style_sheets(cx, this, value)
    }

    fn active_element(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        let focused = if has_window(cx, this) {
            cx.page.document_state.borrow().focused
        } else {
            None
        };
        let dom = cx.dom();
        Ok(focused
            .filter(|&n| dom.contains(n) && dom.is_connected(n))
            .or_else(|| body(&dom, this)))
    }
}
