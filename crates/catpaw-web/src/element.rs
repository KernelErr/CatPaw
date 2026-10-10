//! `Element` and `HTMLElement`, with the objects hanging off them:
//! `DOMTokenList`, `DOMStringMap`, `DOMRect`, and the element interfaces
//! that have state of their own.

use catpaw_dom::{
    Attr, Dom, ElementData, FragmentKind, LocalName, Namespace, NodeId, NodeKind, Prefix, QualName,
    parse_fragment_into, to_html,
};
use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, PromiseRef, Value};

use crate::collections::{self, ListSource};
use crate::generated::{self as web, BooleanOrDoubleOrString, InterfaceId};
use crate::node::{self, qualified_name};
use crate::page::Cx;
use crate::{Web, attributes, events, layout, platform_object};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

fn stale() -> Exception {
    Exception::type_error("Illegal invocation")
}

fn with_element<R>(cx: &Cx<'_>, id: NodeId, f: impl FnOnce(&ElementData) -> R) -> Fallible<R> {
    cx.dom().element(id).map(f).ok_or_else(stale)
}

pub(crate) fn attr_qualified_name(attr: &Attr) -> String {
    match &attr.name.prefix {
        Some(prefix) => format!("{}:{}", prefix, attr.name.local),
        None => attr.name.local.to_string(),
    }
}

// ---- attributes -----------------------------------------------------------

/// The value of the null-namespace attribute `local`.
pub(crate) fn get_attr(cx: &Cx<'_>, el: NodeId, local: &str) -> Option<String> {
    cx.dom().attr(el, local).map(str::to_string)
}

/// Runs the hooks that depend on an attribute's value after it changed.
/// `old` is the value it had, if it existed.
fn attribute_changed(
    cx: &mut Cx<'_>,
    el: NodeId,
    local: &str,
    namespace: Option<&str>,
    old: Option<String>,
) {
    crate::mutation_observer::queue_attribute(cx.page, el, local, namespace, old.as_deref());
    let new = {
        let dom = cx.dom();
        let ns = namespace.unwrap_or_default();
        dom.element(el).and_then(|e| {
            e.attrs
                .iter()
                .find(|a| &*a.name.local == local && &*a.name.ns == ns)
                .map(|a| a.value.to_string())
        })
    };
    crate::custom_elements::attribute_changed(
        cx.page,
        el,
        local,
        namespace,
        old.as_deref(),
        new.as_deref(),
    );
    if local.starts_with("on") {
        events::content_handler_changed(cx.page, el, local);
    }
    if matches!(local, "id" | "name")
        && namespace.unwrap_or_default().is_empty()
        && crate::document::is_nameable(&cx.dom(), el)
        && cx.dom().is_connected(el)
    {
        cx.page.document_names.changed();
    }
    if local == "src" && cx.dom().is_html_element(el, "script") {
        crate::scripting::src_attribute_set(cx, el);
    }
    if matches!(local, "href" | "rel" | "disabled") {
        crate::stylesheets::link_changed(cx.page, el, false);
    }
    if matches!(local, "src" | "srcdoc") && crate::frames::is_frame_element(&cx.dom(), el) {
        crate::frames::iframe_changed(cx.page, el);
    }
    if matches!(local, "width" | "height") && cx.dom().is_html_element(el, "canvas") {
        crate::canvas::size_changed(cx.page, el);
    }
    if matches!(local, "multiple" | "size")
        && namespace.unwrap_or_default().is_empty()
        && cx.dom().is_html_element(el, "select")
    {
        crate::forms::selectedness_reset(cx.page, el);
    }
    // An input that stops being a file input lets go of its files: made
    // one again, it has none chosen.
    if local == "type"
        && namespace.unwrap_or_default().is_empty()
        && cx.dom().is_html_element(el, "input")
        && type_keyword(old.as_deref()) == "file"
        && input_type(&cx.dom(), el) != "file"
    {
        crate::file_api::forget_files(cx, el);
    }
}

/// Sets the null-namespace attribute `local`.
pub(crate) fn set_attr(cx: &mut Cx<'_>, el: NodeId, local: &str, value: String) -> Fallible<()> {
    let old = {
        let mut dom = cx.dom_mut();
        let data = dom.element_mut(el).ok_or_else(stale)?;
        match data
            .attrs
            .iter_mut()
            .find(|a| a.name.ns.is_empty() && &*a.name.local == local)
        {
            Some(existing) => Some(std::mem::replace(&mut existing.value, value)),
            None => {
                data.attrs.push(Attr::html(local, value));
                None
            }
        }
    };
    attribute_changed(cx, el, local, None, old);
    Ok(())
}

pub(crate) fn remove_attr(cx: &mut Cx<'_>, el: NodeId, local: &str) {
    let removed = cx
        .dom_mut()
        .element_mut(el)
        .and_then(|data| data.remove_attr(local));
    if let Some(removed) = removed {
        attribute_changed(cx, el, local, None, Some(removed.value));
    }
}

/// <https://dom.spec.whatwg.org/#valid-attribute-local-name>
pub(crate) fn is_valid_attribute_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|c| {
            matches!(
                c,
                ' ' | '\t' | '\n' | '\x0C' | '\r' | '\0' | '/' | '=' | '>'
            )
        })
}

/// <https://dom.spec.whatwg.org/#valid-element-local-name>
pub(crate) fn is_valid_element_name(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    if first.is_ascii_alphabetic() {
        return !name
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r' | '\0' | '/' | '>'));
    }
    if !(first == ':' || first == '_' || first as u32 >= 0x80) {
        return false;
    }
    name.chars().skip(1).all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_') || c as u32 >= 0x80
    })
}

fn invalid_name(name: &str) -> Exception {
    Exception::invalid_character(format!("'{name}' is not a valid name"))
}

/// <https://dom.spec.whatwg.org/#validate-and-extract>
pub(crate) fn validate_and_extract(
    namespace: Option<String>,
    qualified_name: &str,
    for_element: bool,
) -> Fallible<QualName> {
    let namespace = namespace.filter(|ns| !ns.is_empty());
    let (prefix, local) = match qualified_name.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() => (Some(prefix), local),
        _ => (None, qualified_name),
    };
    let valid = if for_element {
        is_valid_element_name(local)
    } else {
        is_valid_attribute_name(local)
    };
    let prefix_valid = prefix.is_none_or(|p| {
        !p.chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r' | '\0' | '/' | '>'))
    });
    if !valid || !prefix_valid {
        return Err(invalid_name(qualified_name));
    }
    let ns = namespace.as_deref();
    let bad = (prefix.is_some() && ns.is_none())
        || (prefix == Some("xml") && ns != Some(XML_NS))
        || ((qualified_name == "xmlns" || prefix == Some("xmlns")) && ns != Some(XMLNS_NS))
        || (ns == Some(XMLNS_NS) && qualified_name != "xmlns" && prefix != Some("xmlns"));
    if bad {
        return Err(Exception::namespace(format!(
            "'{qualified_name}' is not valid in this namespace"
        )));
    }
    Ok(QualName::new(
        prefix.map(Prefix::from),
        Namespace::from(ns.unwrap_or("")),
        LocalName::from(local),
    ))
}

/// The name to look attributes up by: lowercased for HTML elements.
fn lookup_name(el: &ElementData, qualified_name: &str) -> String {
    if el.is_html() {
        qualified_name.to_ascii_lowercase()
    } else {
        qualified_name.to_string()
    }
}

fn find_attr_ns<'e>(
    el: &'e ElementData,
    namespace: &Option<String>,
    local: &str,
) -> Option<&'e Attr> {
    let ns = namespace.as_deref().unwrap_or("");
    el.attrs
        .iter()
        .find(|a| &*a.name.ns == ns && &*a.name.local == local)
}

// ---- markup ---------------------------------------------------------------

/// The node whose children `innerHTML` reads and replaces: the element, or
/// a template's contents.
fn inner_target(dom: &Dom, el: NodeId) -> NodeId {
    dom.element(el)
        .and_then(|e| e.template_contents)
        .unwrap_or(el)
}

/// Parses `markup` as `context`'s content, for a tree that is not the
/// element's own children (a shadow tree).
pub(crate) fn parse_fragment_for(cx: &Cx<'_>, markup: &str, context: NodeId) -> NodeId {
    parse_fragment(cx, markup, context)
}

fn parse_fragment(cx: &Cx<'_>, markup: &str, context: NodeId) -> NodeId {
    let fragment = parse_fragment_into(&cx.page.dom, markup, context, true);
    crate::custom_elements::subtree_created(cx.page, fragment);
    fragment
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    BeforeBegin,
    AfterBegin,
    BeforeEnd,
    AfterEnd,
}

fn parse_position(position: &str) -> Fallible<Position> {
    Ok(match position.to_ascii_lowercase().as_str() {
        "beforebegin" => Position::BeforeBegin,
        "afterbegin" => Position::AfterBegin,
        "beforeend" => Position::BeforeEnd,
        "afterend" => Position::AfterEnd,
        _ => {
            return Err(Exception::syntax(format!(
                "'{position}' is not a valid position"
            )));
        }
    })
}

/// <https://dom.spec.whatwg.org/#insert-adjacent>
fn insert_adjacent(
    cx: &mut Cx<'_>,
    el: NodeId,
    position: Position,
    node: NodeId,
) -> Fallible<Option<NodeId>> {
    let (parent, first, next) = {
        let dom = cx.dom();
        (dom.parent(el), dom.first_child(el), dom.next_sibling(el))
    };
    match position {
        Position::BeforeBegin => match parent {
            Some(parent) => node::pre_insert(cx, node, parent, Some(el)).map(Some),
            None => Ok(None),
        },
        Position::AfterBegin => node::pre_insert(cx, node, el, first).map(Some),
        Position::BeforeEnd => node::pre_insert(cx, node, el, None).map(Some),
        Position::AfterEnd => match parent {
            Some(parent) => node::pre_insert(cx, node, parent, next).map(Some),
            None => Ok(None),
        },
    }
}

// ---- rendered text --------------------------------------------------------

const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tr",
    "ul",
];

const UNRENDERED_TAGS: &[&str] = &["script", "style", "template", "noscript", "head", "title"];

/// An approximation of `innerText` that needs no layout: text with
/// collapsed whitespace, line breaks at `<br>` and around block-level
/// elements, and nothing from elements that never render.
fn rendered_text(dom: &Dom, root: NodeId) -> String {
    fn walk(dom: &Dom, node: NodeId, preformatted: bool, out: &mut String) {
        for child in dom.children(node) {
            match dom.kind(child) {
                NodeKind::Text(text) => {
                    if preformatted {
                        out.push_str(text);
                        continue;
                    }
                    for c in text.chars() {
                        if c.is_whitespace() {
                            if !out.is_empty() && !out.ends_with([' ', '\n']) {
                                out.push(' ');
                            }
                        } else {
                            out.push(c);
                        }
                    }
                }
                NodeKind::Element(el) => {
                    let local = &*el.name.local;
                    if el.is_html() && (UNRENDERED_TAGS.contains(&local) || el.has_attr("hidden")) {
                        continue;
                    }
                    if el.is_html() && local == "br" {
                        out.push('\n');
                        continue;
                    }
                    let block = el.is_html() && BLOCK_TAGS.contains(&local);
                    let paragraph = el.is_html() && local == "p";
                    if block {
                        break_line(out, paragraph);
                    }
                    let pre = preformatted || (el.is_html() && matches!(local, "pre" | "textarea"));
                    walk(dom, child, pre, out);
                    if block {
                        break_line(out, paragraph);
                    }
                }
                _ => {}
            }
        }
    }

    /// Ends the current line (and, for paragraphs, leaves a blank one).
    fn break_line(out: &mut String, blank: bool) {
        while out.ends_with(' ') {
            out.pop();
        }
        if out.is_empty() {
            return;
        }
        let wanted = if blank { 2 } else { 1 };
        let have = out.chars().rev().take_while(|&c| c == '\n').count();
        for _ in have..wanted {
            out.push('\n');
        }
    }

    let mut out = String::new();
    let preformatted = dom
        .element(root)
        .is_some_and(|e| e.is_html() && matches!(&*e.name.local, "pre" | "textarea"));
    walk(dom, root, preformatted, &mut out);
    out.trim_matches(|c| c == '\n' || c == ' ').to_string()
}

/// The fragment `innerText`/`outerText` setters insert: text with `<br>`
/// elements at line breaks.
fn text_fragment(cx: &Cx<'_>, text: &str) -> NodeId {
    let mut dom = cx.dom_mut();
    let fragment = dom.create_fragment(catpaw_dom::FragmentKind::Plain);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for (i, line) in normalized.split('\n').enumerate() {
        if i > 0 {
            let br = dom.create_html_element("br", Vec::new());
            dom.append_child(fragment, br);
        }
        if !line.is_empty() {
            let t = dom.create_text(line);
            dom.append_child(fragment, t);
        }
    }
    fragment
}

/// The concatenated data of the Text node children (not descendants).
pub(crate) fn child_text_content(dom: &Dom, node: NodeId) -> String {
    dom.children(node)
        .filter_map(|c| dom.node(c).as_text())
        .collect()
}

// ---- focus ----------------------------------------------------------------

pub(crate) fn is_focusable(dom: &Dom, el: NodeId) -> bool {
    let Some(data) = dom.element(el) else {
        return false;
    };
    if !dom.is_connected(el) {
        return false;
    }
    if data.has_attr("tabindex") {
        return true;
    }
    if !data.is_html() {
        return false;
    }
    match &*data.name.local {
        "a" | "area" => data.has_attr("href"),
        "button" | "input" | "select" | "textarea" => !data.has_attr("disabled"),
        "iframe" | "summary" => true,
        _ => data
            .attr("contenteditable")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false")),
    }
}

pub(crate) fn move_focus(cx: &mut Cx<'_>, to: Option<NodeId>) {
    let from = cx.page.document_state.borrow().focused;
    if from == to {
        return;
    }
    cx.page.document_state.borrow_mut().focused = to;
    if let Some(old) = from.filter(|&n| cx.dom().contains(n)) {
        events::fire(cx, EventTargetRef::Node(old), "blur", false, false);
        events::fire(cx, EventTargetRef::Node(old), "focusout", true, false);
    }
    if let Some(new) = to {
        events::fire(cx, EventTargetRef::Node(new), "focus", false, false);
        events::fire(cx, EventTargetRef::Node(new), "focusin", true, false);
    }
}

// ---- objects --------------------------------------------------------------

/// A `DOMTokenList` over one attribute of an element (`classList`, `relList`).
pub struct TokenListObject {
    pub element: NodeId,
    pub attr: String,
}
platform_object!(TokenListObject, DOMTokenList);

/// `element.dataset`.
pub struct StringMapObject {
    pub element: NodeId,
}
platform_object!(StringMapObject, DOMStringMap);

/// `DOMRect` and `DOMRectReadOnly`.
pub struct RectObject {
    pub iface: InterfaceId,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
platform_object!(RectObject, |r| r.iface);

fn token_list(cx: &Cx<'_>, id: ObjectId) -> Fallible<(NodeId, String)> {
    cx.page
        .with::<TokenListObject, _>(id, |t| (t.element, t.attr.clone()))
}

fn tokens(cx: &Cx<'_>, id: ObjectId) -> Fallible<Vec<String>> {
    let (el, attr) = token_list(cx, id)?;
    let mut out: Vec<String> = Vec::new();
    if let Some(value) = cx.dom().attr(el, &attr) {
        for token in value.split_ascii_whitespace() {
            if !out.iter().any(|t| t == token) {
                out.push(token.to_string());
            }
        }
    }
    Ok(out)
}

fn validate_token(token: &str) -> Fallible<()> {
    if token.is_empty() {
        return Err(Exception::syntax("The token must not be empty"));
    }
    if token.contains(|c: char| c.is_ascii_whitespace()) {
        return Err(Exception::invalid_character(
            "The token must not contain whitespace",
        ));
    }
    Ok(())
}

/// <https://dom.spec.whatwg.org/#concept-dtl-update>
fn update_tokens(cx: &mut Cx<'_>, id: ObjectId, tokens: &[String]) -> Fallible<()> {
    let (el, attr) = token_list(cx, id)?;
    if tokens.is_empty() && cx.dom().attr(el, &attr).is_none() {
        return Ok(());
    }
    set_attr(cx, el, &attr, tokens.join(" "))
}

/// `data-foo-bar` → `fooBar`; `None` if the attribute is not a data attribute
/// reachable through `dataset`.
fn dataset_key(attr_local: &str) -> Option<String> {
    let rest = attr_local.strip_prefix("data-")?;
    if rest.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek().is_some_and(char::is_ascii_lowercase) {
            out.push(chars.next().unwrap_or(c).to_ascii_uppercase());
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// `fooBar` → `data-foo-bar`.
fn dataset_attr(key: &str) -> String {
    let mut out = String::from("data-");
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A finite scroll coordinate; NaN and infinities count as zero.
fn finite_or_zero(value: f64) -> f32 {
    if value.is_finite() { value as f32 } else { 0.0 }
}

fn resolved_promise(cx: &mut Cx<'_>) -> PromiseRef {
    let promise = cx.script.new_promise();
    cx.script.resolve_promise(&promise, Value::Undefined);
    promise
}

// ---------------------------------------------------------------- bindings

impl web::ElementImpl for Web {
    fn get_bounding_client_rect(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        let rect = layout::bounding_client_rect(cx.page, this);
        Ok(layout::rect_object(cx.page, rect))
    }

    fn get_client_rects(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        let rects = layout::client_rects(cx.page, this);
        Ok(layout::rect_list(cx.page, rects))
    }

    fn scroll_into_view(
        cx: &mut Cx<'_>,
        this: NodeId,
        arg: web::BooleanOrScrollIntoViewOptions,
    ) -> Fallible<PromiseRef> {
        let (block, inline) = match arg {
            web::BooleanOrScrollIntoViewOptions::Boolean(true) => (
                web::ScrollLogicalPosition::Start,
                web::ScrollLogicalPosition::Nearest,
            ),
            web::BooleanOrScrollIntoViewOptions::Boolean(false) => (
                web::ScrollLogicalPosition::End,
                web::ScrollLogicalPosition::Nearest,
            ),
            web::BooleanOrScrollIntoViewOptions::ScrollIntoViewOptions(options) => {
                (options.block, options.inline)
            }
        };
        layout::scroll_into_view(cx, this, block, inline);
        Ok(resolved_promise(cx))
    }

    fn scroll(
        cx: &mut Cx<'_>,
        this: NodeId,
        options: web::ScrollToOptions,
    ) -> Fallible<PromiseRef> {
        <Self as web::ElementImpl>::scroll_to(cx, this, options)
    }

    fn scroll_overload2(cx: &mut Cx<'_>, this: NodeId, x: f64, y: f64) -> Fallible<PromiseRef> {
        <Self as web::ElementImpl>::scroll_to_overload2(cx, this, x, y)
    }

    fn scroll_to(
        cx: &mut Cx<'_>,
        this: NodeId,
        options: web::ScrollToOptions,
    ) -> Fallible<PromiseRef> {
        let (x, y) = layout::scroll_position(cx.page, this);
        let x = options.left.map_or(x, finite_or_zero);
        let y = options.top.map_or(y, finite_or_zero);
        layout::scroll_element_to(cx, this, x, y);
        Ok(resolved_promise(cx))
    }

    fn scroll_to_overload2(cx: &mut Cx<'_>, this: NodeId, x: f64, y: f64) -> Fallible<PromiseRef> {
        layout::scroll_element_to(cx, this, finite_or_zero(x), finite_or_zero(y));
        Ok(resolved_promise(cx))
    }

    fn scroll_by(
        cx: &mut Cx<'_>,
        this: NodeId,
        options: web::ScrollToOptions,
    ) -> Fallible<PromiseRef> {
        let (x, y) = layout::scroll_position(cx.page, this);
        let dx = options.left.map_or(0.0, finite_or_zero);
        let dy = options.top.map_or(0.0, finite_or_zero);
        layout::scroll_element_to(cx, this, x + dx, y + dy);
        Ok(resolved_promise(cx))
    }

    fn scroll_by_overload2(cx: &mut Cx<'_>, this: NodeId, x: f64, y: f64) -> Fallible<PromiseRef> {
        let (cur_x, cur_y) = layout::scroll_position(cx.page, this);
        layout::scroll_element_to(
            cx,
            this,
            cur_x + finite_or_zero(x),
            cur_y + finite_or_zero(y),
        );
        Ok(resolved_promise(cx))
    }

    fn scroll_top(cx: &mut Cx<'_>, this: NodeId) -> Fallible<f64> {
        Ok(f64::from(layout::scroll_position(cx.page, this).1))
    }

    fn set_scroll_top(cx: &mut Cx<'_>, this: NodeId, value: f64) -> Fallible<()> {
        let (x, _) = layout::scroll_position(cx.page, this);
        layout::scroll_element_to(cx, this, x, finite_or_zero(value));
        Ok(())
    }

    fn scroll_left(cx: &mut Cx<'_>, this: NodeId) -> Fallible<f64> {
        Ok(f64::from(layout::scroll_position(cx.page, this).0))
    }

    fn set_scroll_left(cx: &mut Cx<'_>, this: NodeId, value: f64) -> Fallible<()> {
        let (_, y) = layout::scroll_position(cx.page, this);
        layout::scroll_element_to(cx, this, finite_or_zero(value), y);
        Ok(())
    }

    fn scroll_width(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::scroll_size(cx.page, this).0.round() as i32)
    }

    fn scroll_height(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::scroll_size(cx.page, this).1.round() as i32)
    }

    fn client_top(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::client_box(cx.page, this).y.round() as i32)
    }

    fn client_left(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::client_box(cx.page, this).x.round() as i32)
    }

    fn client_width(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::client_box(cx.page, this).width.round() as i32)
    }

    fn client_height(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::client_box(cx.page, this).height.round() as i32)
    }

    fn namespace_uri(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<String>> {
        with_element(cx, this, |el| {
            (!el.name.ns.is_empty()).then(|| el.name.ns.to_string())
        })
    }

    fn prefix(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<String>> {
        with_element(cx, this, |el| {
            el.name.prefix.as_ref().map(|p| p.to_string())
        })
    }

    fn local_name(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        with_element(cx, this, |el| el.name.local.to_string())
    }

    fn tag_name(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        let html_document = node::in_html_document(&cx.dom(), this);
        with_element(cx, this, |el| {
            let name = qualified_name(el);
            if el.is_html() && html_document {
                name.to_ascii_uppercase()
            } else {
                name
            }
        })
    }

    fn id(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        with_element(cx, this, |el| el.attr("id").unwrap_or_default().to_string())
    }

    fn set_id(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        set_attr(cx, this, "id", value)
    }

    fn class_name(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        with_element(cx, this, |el| {
            el.attr("class").unwrap_or_default().to_string()
        })
    }

    fn set_class_name(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        set_attr(cx, this, "class", value)
    }

    fn class_list(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(TokenListObject {
            element: this,
            attr: "class".to_string(),
        }))
    }

    fn slot(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        with_element(cx, this, |el| {
            el.attr("slot").unwrap_or_default().to_string()
        })
    }

    fn set_slot(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        set_attr(cx, this, "slot", value)
    }

    fn has_attributes(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        with_element(cx, this, |el| !el.attrs.is_empty())
    }

    fn get_attribute_names(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Vec<String>> {
        with_element(cx, this, |el| {
            el.attrs.iter().map(attr_qualified_name).collect()
        })
    }

    fn get_attribute(
        cx: &mut Cx<'_>,
        this: NodeId,
        qualified_name: String,
    ) -> Fallible<Option<String>> {
        with_element(cx, this, |el| {
            let name = lookup_name(el, &qualified_name);
            el.attrs
                .iter()
                .find(|a| attr_qualified_name(a) == name)
                .map(|a| a.value.clone())
        })
    }

    fn get_attribute_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        local_name: String,
    ) -> Fallible<Option<String>> {
        let namespace = namespace.filter(|ns| !ns.is_empty());
        with_element(cx, this, |el| {
            find_attr_ns(el, &namespace, &local_name).map(|a| a.value.clone())
        })
    }

    fn attach_shadow(cx: &mut Cx<'_>, this: NodeId, init: web::ShadowRootInit) -> Fallible<NodeId> {
        crate::shadow::attach(cx, this, init)
    }

    fn shadow_root(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        let dom = cx.dom();
        let Some(shadow) = dom.element(this).and_then(|e| e.shadow_root) else {
            return Ok(None);
        };
        let open = matches!(
            dom.kind(shadow),
            NodeKind::DocumentFragment(FragmentKind::ShadowRoot { open: true, .. })
        );
        Ok(open.then_some(shadow))
    }

    fn attributes(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(attributes::map(cx, this))
    }

    fn get_attribute_node(
        cx: &mut Cx<'_>,
        this: NodeId,
        qualified_name: String,
    ) -> Fallible<Option<ObjectId>> {
        node::check(cx, this)?;
        Ok(attributes::get(cx, this, &qualified_name))
    }

    fn get_attribute_node_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        local_name: String,
    ) -> Fallible<Option<ObjectId>> {
        node::check(cx, this)?;
        let namespace = namespace.filter(|ns| !ns.is_empty());
        Ok(attributes::get_ns(
            cx,
            this,
            namespace.as_deref(),
            &local_name,
        ))
    }

    fn set_attribute_node(
        cx: &mut Cx<'_>,
        this: NodeId,
        attr: ObjectId,
    ) -> Fallible<Option<ObjectId>> {
        attributes::set(cx, this, attr)
    }

    fn set_attribute_node_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        attr: ObjectId,
    ) -> Fallible<Option<ObjectId>> {
        attributes::set(cx, this, attr)
    }

    fn remove_attribute_node(cx: &mut Cx<'_>, this: NodeId, attr: ObjectId) -> Fallible<ObjectId> {
        attributes::remove(cx, this, attr)
    }

    fn set_attribute(
        cx: &mut Cx<'_>,
        this: NodeId,
        qualified_name: String,
        value: String,
    ) -> Fallible<()> {
        if !is_valid_attribute_name(&qualified_name) {
            return Err(invalid_name(&qualified_name));
        }
        let (local, namespace, old) = {
            let mut dom = cx.dom_mut();
            let el = dom.element_mut(this).ok_or_else(stale)?;
            let name = lookup_name(el, &qualified_name);
            match el.attrs.iter_mut().find(|a| attr_qualified_name(a) == name) {
                Some(existing) => {
                    let old = std::mem::replace(&mut existing.value, value);
                    let namespace =
                        (!existing.name.ns.is_empty()).then(|| existing.name.ns.to_string());
                    (existing.name.local.to_string(), namespace, Some(old))
                }
                None => {
                    el.attrs.push(Attr::html(&name, value));
                    (name, None, None)
                }
            }
        };
        attribute_changed(cx, this, &local, namespace.as_deref(), old);
        Ok(())
    }

    fn set_attribute_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        qualified_name: String,
        value: String,
    ) -> Fallible<()> {
        let name = validate_and_extract(namespace, &qualified_name, false)?;
        let local = name.local.to_string();
        let namespace = (!name.ns.is_empty()).then(|| name.ns.to_string());
        let old = {
            let mut dom = cx.dom_mut();
            let el = dom.element_mut(this).ok_or_else(stale)?;
            match el
                .attrs
                .iter_mut()
                .find(|a| a.name.ns == name.ns && a.name.local == name.local)
            {
                Some(existing) => Some(std::mem::replace(&mut existing.value, value)),
                None => {
                    el.attrs.push(Attr::new(name, value));
                    None
                }
            }
        };
        attribute_changed(cx, this, &local, namespace.as_deref(), old);
        Ok(())
    }

    fn remove_attribute(cx: &mut Cx<'_>, this: NodeId, qualified_name: String) -> Fallible<()> {
        let removed = {
            let mut dom = cx.dom_mut();
            let el = dom.element_mut(this).ok_or_else(stale)?;
            let name = lookup_name(el, &qualified_name);
            el.attrs
                .iter()
                .position(|a| attr_qualified_name(a) == name)
                .map(|i| el.attrs.remove(i))
        };
        if let Some(removed) = removed {
            let namespace = (!removed.name.ns.is_empty()).then(|| removed.name.ns.to_string());
            attribute_changed(
                cx,
                this,
                &removed.name.local,
                namespace.as_deref(),
                Some(removed.value),
            );
        }
        Ok(())
    }

    fn remove_attribute_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        local_name: String,
    ) -> Fallible<()> {
        let ns = namespace.unwrap_or_default();
        let removed = {
            let mut dom = cx.dom_mut();
            let el = dom.element_mut(this).ok_or_else(stale)?;
            el.attrs
                .iter()
                .position(|a| *a.name.ns == ns && *a.name.local == local_name)
                .map(|i| el.attrs.remove(i))
        };
        if let Some(removed) = removed {
            let namespace = (!ns.is_empty()).then_some(ns.as_str());
            attribute_changed(cx, this, &local_name, namespace, Some(removed.value));
        }
        Ok(())
    }

    fn toggle_attribute(
        cx: &mut Cx<'_>,
        this: NodeId,
        qualified_name: String,
        force: Option<bool>,
    ) -> Fallible<bool> {
        if !is_valid_attribute_name(&qualified_name) {
            return Err(invalid_name(&qualified_name));
        }
        let present = <Web as web::ElementImpl>::has_attribute(cx, this, qualified_name.clone())?;
        if present {
            if force == Some(true) {
                return Ok(true);
            }
            <Web as web::ElementImpl>::remove_attribute(cx, this, qualified_name)?;
            Ok(false)
        } else {
            if force == Some(false) {
                return Ok(false);
            }
            <Web as web::ElementImpl>::set_attribute(cx, this, qualified_name, String::new())?;
            Ok(true)
        }
    }

    fn has_attribute(cx: &mut Cx<'_>, this: NodeId, qualified_name: String) -> Fallible<bool> {
        with_element(cx, this, |el| {
            let name = lookup_name(el, &qualified_name);
            el.attrs.iter().any(|a| attr_qualified_name(a) == name)
        })
    }

    fn has_attribute_ns(
        cx: &mut Cx<'_>,
        this: NodeId,
        namespace: Option<String>,
        local_name: String,
    ) -> Fallible<bool> {
        let namespace = namespace.filter(|ns| !ns.is_empty());
        with_element(cx, this, |el| {
            find_attr_ns(el, &namespace, &local_name).is_some()
        })
    }

    fn closest(cx: &mut Cx<'_>, this: NodeId, selectors: String) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        let parsed = node::parse_selectors(&selectors)?;
        Ok(catpaw_style::query::closest(&cx.dom(), this, &parsed))
    }

    fn matches(cx: &mut Cx<'_>, this: NodeId, selectors: String) -> Fallible<bool> {
        node::check(cx, this)?;
        let parsed = node::parse_selectors(&selectors)?;
        Ok(catpaw_style::query::matches(&cx.dom(), this, &parsed))
    }

    fn webkit_matches_selector(cx: &mut Cx<'_>, this: NodeId, selectors: String) -> Fallible<bool> {
        <Web as web::ElementImpl>::matches(cx, this, selectors)
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

    fn insert_adjacent_element(
        cx: &mut Cx<'_>,
        this: NodeId,
        where_: String,
        element: NodeId,
    ) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        let position = parse_position(&where_)?;
        insert_adjacent(cx, this, position, element)
    }

    fn insert_adjacent_text(
        cx: &mut Cx<'_>,
        this: NodeId,
        where_: String,
        data: String,
    ) -> Fallible<()> {
        node::check(cx, this)?;
        let position = parse_position(&where_)?;
        let text = cx.dom_mut().create_text(data);
        insert_adjacent(cx, this, position, text).map(drop)
    }

    fn inner_html(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        let dom = cx.dom();
        let target = inner_target(&dom, this);
        Ok(if target == this {
            to_html(&dom, this, true)
        } else {
            to_html(&dom, target, true)
        })
    }

    fn set_inner_html(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        let fragment = parse_fragment(cx, &value, this);
        let target = inner_target(&cx.dom(), this);
        node::replace_all(cx, Some(fragment), target);
        Ok(())
    }

    fn outer_html(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(to_html(&cx.dom(), this, false))
    }

    fn set_outer_html(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        let Some(parent) = cx.dom().parent(this) else {
            return Ok(());
        };
        let context = {
            let mut dom = cx.dom_mut();
            match dom.kind(parent) {
                NodeKind::Document(_) => {
                    return Err(Exception::no_modification_allowed(
                        "The element's parent is a document",
                    ));
                }
                NodeKind::Element(_) => parent,
                _ => dom.create_html_element("body", Vec::new()),
            }
        };
        let fragment = parse_fragment(cx, &value, context);
        node::replace(cx, this, fragment, parent).map(drop)
    }

    fn insert_adjacent_html(
        cx: &mut Cx<'_>,
        this: NodeId,
        position: String,
        string: String,
    ) -> Fallible<()> {
        node::check(cx, this)?;
        let position = parse_position(&position)?;
        let context = match position {
            Position::BeforeBegin | Position::AfterEnd => {
                let dom = cx.dom();
                match dom.parent(this) {
                    Some(parent) if dom.is_element(parent) => parent,
                    _ => {
                        return Err(Exception::no_modification_allowed(
                            "The element has no parent element",
                        ));
                    }
                }
            }
            Position::AfterBegin | Position::BeforeEnd => this,
        };
        let fragment = parse_fragment(cx, &string, context);
        insert_adjacent(cx, this, position, fragment).map(drop)
    }
}

impl web::HTMLElementImpl for Web {
    fn offset_parent(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(layout::offsets(cx.page, this).parent)
    }

    fn offset_top(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::offsets(cx.page, this).top.round() as i32)
    }

    fn offset_left(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::offsets(cx.page, this).left.round() as i32)
    }

    fn offset_width(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::offsets(cx.page, this).width.round() as i32)
    }

    fn offset_height(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        Ok(layout::offsets(cx.page, this).height.round() as i32)
    }

    fn hidden(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<BooleanOrDoubleOrString>> {
        Ok(Some(match get_attr(cx, this, "hidden") {
            None => BooleanOrDoubleOrString::Boolean(false),
            Some(v) if v.eq_ignore_ascii_case("until-found") => {
                BooleanOrDoubleOrString::String("until-found".to_string())
            }
            Some(_) => BooleanOrDoubleOrString::Boolean(true),
        }))
    }

    fn set_hidden(
        cx: &mut Cx<'_>,
        this: NodeId,
        value: Option<BooleanOrDoubleOrString>,
    ) -> Fallible<()> {
        let new = match value {
            Some(BooleanOrDoubleOrString::String(s)) if s.eq_ignore_ascii_case("until-found") => {
                Some("until-found")
            }
            Some(BooleanOrDoubleOrString::Boolean(true)) => Some(""),
            Some(BooleanOrDoubleOrString::String(s)) if !s.is_empty() => Some(""),
            Some(BooleanOrDoubleOrString::Double(d)) if d != 0.0 && !d.is_nan() => Some(""),
            _ => None,
        };
        match new {
            Some(v) => set_attr(cx, this, "hidden", v.to_string()),
            None => {
                remove_attr(cx, this, "hidden");
                Ok(())
            }
        }
    }

    fn click(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        node::check(cx, this)?;
        crate::activation::click(cx, this, false);
        Ok(())
    }

    fn inner_text(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(rendered_text(&cx.dom(), this))
    }

    fn set_inner_text(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        let fragment = text_fragment(cx, &value);
        node::replace_all(cx, Some(fragment), this);
        Ok(())
    }

    fn outer_text(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        <Web as web::HTMLElementImpl>::inner_text(cx, this)
    }

    fn set_outer_text(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        let Some(parent) = cx.dom().parent(this) else {
            return Err(Exception::no_modification_allowed(
                "The element has no parent",
            ));
        };
        let fragment = text_fragment(cx, &value);
        // An empty replacement still leaves an (empty) text node behind.
        if !cx.dom().has_children(fragment) {
            let mut dom = cx.dom_mut();
            let empty = dom.create_text("");
            dom.append_child(fragment, empty);
        }
        node::replace(cx, this, fragment, parent).map(drop)
    }
}

impl web::HTMLOrSVGOrMathMLElementImpl for Web {
    fn dataset(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(StringMapObject { element: this }))
    }

    fn focus(cx: &mut Cx<'_>, this: NodeId, _options: web::FocusOptions) -> Fallible<()> {
        node::check(cx, this)?;
        if is_focusable(&cx.dom(), this) {
            move_focus(cx, Some(this));
        }
        Ok(())
    }

    fn blur(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        if cx.page.document_state.borrow().focused == Some(this) {
            move_focus(cx, None);
        }
        Ok(())
    }
}

impl web::DOMTokenListImpl for Web {
    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        Ok(tokens(cx, this)?.len() as u32)
    }

    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<String>> {
        Ok(tokens(cx, this)?.into_iter().nth(index as usize))
    }

    fn contains(cx: &mut Cx<'_>, this: ObjectId, token: String) -> Fallible<bool> {
        Ok(tokens(cx, this)?.contains(&token))
    }

    fn add(cx: &mut Cx<'_>, this: ObjectId, new_tokens: Vec<String>) -> Fallible<()> {
        for t in &new_tokens {
            validate_token(t)?;
        }
        let mut current = tokens(cx, this)?;
        for t in new_tokens {
            if !current.contains(&t) {
                current.push(t);
            }
        }
        update_tokens(cx, this, &current)
    }

    fn remove(cx: &mut Cx<'_>, this: ObjectId, old_tokens: Vec<String>) -> Fallible<()> {
        for t in &old_tokens {
            validate_token(t)?;
        }
        let mut current = tokens(cx, this)?;
        current.retain(|t| !old_tokens.contains(t));
        update_tokens(cx, this, &current)
    }

    fn toggle(
        cx: &mut Cx<'_>,
        this: ObjectId,
        token: String,
        force: Option<bool>,
    ) -> Fallible<bool> {
        validate_token(&token)?;
        let mut current = tokens(cx, this)?;
        if let Some(i) = current.iter().position(|t| *t == token) {
            if force == Some(true) {
                return Ok(true);
            }
            current.remove(i);
            update_tokens(cx, this, &current)?;
            Ok(false)
        } else {
            if force == Some(false) {
                return Ok(false);
            }
            current.push(token);
            update_tokens(cx, this, &current)?;
            Ok(true)
        }
    }

    fn replace(
        cx: &mut Cx<'_>,
        this: ObjectId,
        token: String,
        new_token: String,
    ) -> Fallible<bool> {
        validate_token(&token)?;
        validate_token(&new_token)?;
        let mut current = tokens(cx, this)?;
        let Some(i) = current.iter().position(|t| *t == token) else {
            return Ok(false);
        };
        if let Some(j) = current.iter().position(|t| *t == new_token) {
            // Keep the earlier of the two positions (one when the tokens
            // are the same).
            if i != j {
                current.remove(i.max(j));
            }
            current[i.min(j)] = new_token;
        } else {
            current[i] = new_token;
        }
        update_tokens(cx, this, &current)?;
        Ok(true)
    }

    fn supports(cx: &mut Cx<'_>, this: ObjectId, token: String) -> Fallible<bool> {
        let (el, attr) = token_list(cx, this)?;
        let dom = cx.dom();
        let local = dom
            .element(el)
            .filter(|e| e.is_html())
            .map(|e| e.name.local.to_string())
            .unwrap_or_default();
        match (attr.as_str(), local.as_str()) {
            // The link types this browser acts on. Pages use this to decide
            // whether to polyfill preloading; nothing is preloaded here.
            ("rel", "link") => Ok(token.eq_ignore_ascii_case("stylesheet")),
            ("rel", "a" | "area" | "form") | ("sandbox", "iframe") => Ok(false),
            _ => Err(Exception::type_error(
                "This token list has no supported tokens",
            )),
        }
    }

    fn value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        let (el, attr) = token_list(cx, this)?;
        Ok(get_attr(cx, el, &attr).unwrap_or_default())
    }

    fn set_value(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let (el, attr) = token_list(cx, this)?;
        set_attr(cx, el, &attr, value)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<String>> {
        <Web as web::DOMTokenListImpl>::item(cx, this, index)
    }
}

fn dataset_element(cx: &Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
    cx.page.with::<StringMapObject, _>(this, |m| m.element)
}

impl web::DOMStringMapImpl for Web {
    fn named_get(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<Option<String>> {
        let el = dataset_element(cx, this)?;
        Ok(get_attr(cx, el, &dataset_attr(name)))
    }

    fn named_properties(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<String>> {
        let el = dataset_element(cx, this)?;
        Ok(cx
            .dom()
            .element(el)
            .map(|data| {
                data.attrs
                    .iter()
                    .filter(|a| a.name.ns.is_empty())
                    .filter_map(|a| dataset_key(&a.name.local))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn named_set(cx: &mut Cx<'_>, this: ObjectId, name: &str, value: String) -> Fallible<()> {
        let el = dataset_element(cx, this)?;
        let bytes = name.as_bytes();
        let dash_lower = bytes
            .windows(2)
            .any(|w| w[0] == b'-' && w[1].is_ascii_lowercase());
        if dash_lower {
            return Err(Exception::syntax(format!(
                "'{name}' is not a valid property name"
            )));
        }
        let attr = dataset_attr(name);
        if !is_valid_attribute_name(&attr) {
            return Err(invalid_name(&attr));
        }
        set_attr(cx, el, &attr, value)
    }

    fn named_delete(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<bool> {
        let el = dataset_element(cx, this)?;
        remove_attr(cx, el, &dataset_attr(name));
        Ok(true)
    }
}

fn rect<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut RectObject) -> R) -> Fallible<R> {
    cx.page.with::<RectObject, _>(this, f)
}

impl web::DOMRectReadOnlyImpl for Web {
    fn x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.x)
    }

    fn y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.y)
    }

    fn width(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.width)
    }

    fn height(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.height)
    }

    fn top(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.y.min(r.y + r.height))
    }

    fn right(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.x.max(r.x + r.width))
    }

    fn bottom(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.y.max(r.y + r.height))
    }

    fn left(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.x.min(r.x + r.width))
    }

    fn to_json(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        rect(cx, this, |r| {
            let n = Value::Number;
            Value::Record(vec![
                ("x".to_string(), n(r.x)),
                ("y".to_string(), n(r.y)),
                ("width".to_string(), n(r.width)),
                ("height".to_string(), n(r.height)),
                ("top".to_string(), n(r.y.min(r.y + r.height))),
                ("right".to_string(), n(r.x.max(r.x + r.width))),
                ("bottom".to_string(), n(r.y.max(r.y + r.height))),
                ("left".to_string(), n(r.x.min(r.x + r.width))),
            ])
        })
    }
}

impl web::DOMRectImpl for Web {
    fn x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.x)
    }

    fn set_x(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        rect(cx, this, |r| r.x = value)
    }

    fn y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.y)
    }

    fn set_y(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        rect(cx, this, |r| r.y = value)
    }

    fn width(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.width)
    }

    fn set_width(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        rect(cx, this, |r| r.width = value)
    }

    fn height(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        rect(cx, this, |r| r.height)
    }

    fn set_height(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        rect(cx, this, |r| r.height = value)
    }

    fn constructor(cx: &mut Cx<'_>, x: f64, y: f64, width: f64, height: f64) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(RectObject {
            iface: InterfaceId::DOMRect,
            x,
            y,
            width,
            height,
        }))
    }
}

// ---- form controls and other stateful elements ----------------------------

/// The `type` keywords of `input`; anything else is the text state.
const INPUT_TYPES: &[&str] = &[
    "hidden",
    "text",
    "search",
    "tel",
    "url",
    "email",
    "password",
    "date",
    "month",
    "week",
    "time",
    "datetime-local",
    "number",
    "range",
    "color",
    "checkbox",
    "radio",
    "file",
    "submit",
    "image",
    "reset",
    "button",
];

/// An `input`'s type: its `type` attribute when that is a known keyword
/// (ASCII case-insensitively), else `text`.
fn input_type(dom: &Dom, el: NodeId) -> String {
    type_keyword(dom.attr(el, "type"))
}

/// The type an `input` has with `type` attribute `value` (or none).
fn type_keyword(value: Option<&str>) -> String {
    value
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| INPUT_TYPES.contains(&t.as_str()))
        .unwrap_or_else(|| "text".to_string())
}

/// The current checkedness of a checkbox or radio button.
pub(crate) fn is_checked(cx: &Cx<'_>, el: NodeId) -> bool {
    let dirty = cx.page.form_state.borrow().get(&el).and_then(|s| s.checked);
    dirty.unwrap_or_else(|| cx.dom().attr(el, "checked").is_some())
}

pub(crate) fn set_checked(cx: &Cx<'_>, el: NodeId, value: bool) {
    cx.page
        .form_state
        .borrow_mut()
        .entry(el)
        .or_default()
        .checked = Some(value);
    if !value {
        return;
    }
    // Checking a radio button unchecks the others in its group.
    let group = radio_group(&cx.dom(), el);
    let mut state = cx.page.form_state.borrow_mut();
    for other in group {
        state.entry(other).or_default().checked = Some(false);
    }
}

/// The other radio buttons of a radio button's group (same name, same
/// form owner, same tree); empty for other elements.
fn radio_group(dom: &Dom, el: NodeId) -> Vec<NodeId> {
    if input_type(dom, el) != "radio" {
        return Vec::new();
    }
    let Some(name) = dom.attr(el, "name").filter(|n| !n.is_empty()) else {
        return Vec::new();
    };
    let owner = |n: NodeId| dom.ancestors(n).find(|&a| dom.is_html_element(a, "form"));
    let form = owner(el);
    let root = dom.root_of(el);
    dom.descendants(root)
        .filter(|&n| {
            n != el
                && dom.is_html_element(n, "input")
                && input_type(dom, n) == "radio"
                && dom.attr(n, "name") == Some(name)
                && owner(n) == form
        })
        .collect()
}

/// The radio button of `el`'s group that is checked, other than `el`.
pub(crate) fn checked_in_group(cx: &Cx<'_>, el: NodeId) -> Option<NodeId> {
    let group = radio_group(&cx.dom(), el);
    group.into_iter().find(|&other| is_checked(cx, other))
}

impl web::HTMLInputElementImpl for Web {
    fn form(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        Ok(crate::forms::form_owner(&cx.dom(), this))
    }

    fn files(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<ObjectId>> {
        node::check(cx, this)?;
        if input_type(&cx.dom(), this) != "file" {
            return Ok(None);
        }
        Ok(Some(crate::file_api::file_list(cx, this)))
    }

    /// Takes another `FileList` (as `input.files = dataTransfer.files`
    /// does); `null` changes nothing.
    fn set_files(cx: &mut Cx<'_>, this: NodeId, value: Option<ObjectId>) -> Fallible<()> {
        node::check(cx, this)?;
        if let Some(list) = value
            && input_type(&cx.dom(), this) == "file"
        {
            crate::file_api::set_file_list(cx, this, Some(list));
        }
        Ok(())
    }

    fn type_(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(input_type(&cx.dom(), this))
    }

    fn set_type(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        set_attr(cx, this, "type", value)
    }

    fn default_value(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(get_attr(cx, this, "value").unwrap_or_default())
    }

    fn set_default_value(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        set_attr(cx, this, "value", value)
    }

    fn select(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        node::check(cx, this)?;
        // Selection within a text control is not modelled yet; selecting
        // it all is what typing into a focused control assumes anyway.
        Ok(())
    }

    fn checked(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        node::check(cx, this)?;
        Ok(is_checked(cx, this))
    }

    fn set_checked(cx: &mut Cx<'_>, this: NodeId, value: bool) -> Fallible<()> {
        node::check(cx, this)?;
        set_checked(cx, this, value);
        Ok(())
    }

    fn value(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        if input_type(&cx.dom(), this) == "file" {
            // The first file's name behind the path browsers make up.
            let first = crate::file_api::chosen_files(cx, this).first().copied();
            return Ok(first
                .and_then(|f| crate::file_api::file_name(cx, f))
                .map(|name| format!("C:\\fakepath\\{name}"))
                .unwrap_or_default());
        }
        let dirty = cx
            .page
            .form_state
            .borrow()
            .get(&this)
            .and_then(|s| s.value.clone());
        if let Some(value) = dirty {
            return Ok(value);
        }
        let dom = cx.dom();
        Ok(match dom.attr(this, "value") {
            Some(v) => v.to_string(),
            None if matches!(input_type(&dom, this).as_str(), "checkbox" | "radio") => {
                "on".to_string()
            }
            None => String::new(),
        })
    }

    fn set_value(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        let ty = input_type(&cx.dom(), this);
        match ty.as_str() {
            // These types reflect the content attribute directly.
            "checkbox" | "radio" | "hidden" | "submit" | "image" | "reset" | "button" => {
                set_attr(cx, this, "value", value)
            }
            "file" if !value.is_empty() => Err(Exception::invalid_state(
                "A file input's value can only be set to the empty string",
            )),
            "file" => {
                crate::file_api::set_file_list(cx, this, None);
                Ok(())
            }
            _ => {
                cx.page
                    .form_state
                    .borrow_mut()
                    .entry(this)
                    .or_default()
                    .value = Some(value);
                Ok(())
            }
        }
    }
}

impl web::HTMLTextAreaElementImpl for Web {
    fn form(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        Ok(crate::forms::form_owner(&cx.dom(), this))
    }

    fn type_(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok("textarea".to_string())
    }

    fn default_value(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(child_text_content(&cx.dom(), this))
    }

    fn set_default_value(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        <Web as web::NodeImpl>::set_text_content(cx, this, Some(value))
    }

    fn select(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        node::check(cx, this)?;
        Ok(())
    }

    fn value(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        let dirty = cx
            .page
            .form_state
            .borrow()
            .get(&this)
            .and_then(|s| s.value.clone());
        Ok(match dirty {
            Some(value) => value,
            None => {
                let text = child_text_content(&cx.dom(), this);
                // The parser keeps a leading newline out of the tree already.
                text.replace("\r\n", "\n").replace('\r', "\n")
            }
        })
    }

    fn set_value(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        cx.page
            .form_state
            .borrow_mut()
            .entry(this)
            .or_default()
            .value = Some(value);
        Ok(())
    }
}

impl web::HTMLScriptElementImpl for Web {
    fn text(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(child_text_content(&cx.dom(), this))
    }

    fn set_text(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        node::string_replace_all(cx, &value, this);
        Ok(())
    }
}

impl web::HTMLTitleElementImpl for Web {
    fn text(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(child_text_content(&cx.dom(), this))
    }

    fn set_text(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        node::string_replace_all(cx, &value, this);
        Ok(())
    }
}

impl web::HTMLAnchorElementImpl for Web {
    fn text(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(cx.dom().text_content(this))
    }

    fn set_text(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        node::string_replace_all(cx, &value, this);
        Ok(())
    }
}

impl web::HTMLTemplateElementImpl for Web {
    fn content(cx: &mut Cx<'_>, this: NodeId) -> Fallible<NodeId> {
        with_element(cx, this, |el| el.template_contents)?.ok_or_else(stale)
    }
}

/// The interface a node's script wrapper implements.
pub fn interface_for_node(dom: &Dom, id: NodeId) -> InterfaceId {
    match dom.kind(id) {
        NodeKind::Document(data) if data.is_xml => InterfaceId::XMLDocument,
        NodeKind::Document(_) => InterfaceId::Document,
        NodeKind::Doctype(_) => InterfaceId::DocumentType,
        NodeKind::Text(_) => InterfaceId::Text,
        NodeKind::Comment(_) => InterfaceId::Comment,
        NodeKind::ProcessingInstruction { .. } => InterfaceId::ProcessingInstruction,
        NodeKind::DocumentFragment(FragmentKind::ShadowRoot { .. }) => InterfaceId::ShadowRoot,
        NodeKind::DocumentFragment(_) => InterfaceId::DocumentFragment,
        NodeKind::Element(el) => {
            if &*el.name.ns == crate::svg::SVG_NS {
                return InterfaceId::for_svg_tag(&el.name.local).unwrap_or(InterfaceId::SVGElement);
            }
            if &*el.name.ns != HTML_NS {
                return InterfaceId::Element;
            }
            let local = &*el.name.local;
            if let Some(iface) = crate::custom_elements::interface_of_failed(dom, id) {
                return iface;
            }
            if let Some(iface) = InterfaceId::for_html_tag(local) {
                iface
            } else if crate::html_names::is_known_html_element(local) || local.contains('-') {
                InterfaceId::HTMLElement
            } else {
                InterfaceId::HTMLUnknownElement
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_names_round_trip() {
        assert_eq!(dataset_key("data-foo-bar").as_deref(), Some("fooBar"));
        assert_eq!(dataset_key("data-x").as_deref(), Some("x"));
        assert_eq!(dataset_key("data-"), Some(String::new()));
        assert_eq!(dataset_key("data-Foo"), None);
        assert_eq!(dataset_key("class"), None);
        assert_eq!(dataset_key("data-a-1").as_deref(), Some("a-1"));
        assert_eq!(dataset_attr("fooBar"), "data-foo-bar");
        assert_eq!(dataset_attr("x"), "data-x");
    }

    #[test]
    fn validates_names() {
        assert!(is_valid_element_name("div"));
        assert!(is_valid_element_name("my-element"));
        assert!(is_valid_element_name("_x"));
        assert!(!is_valid_element_name(""));
        assert!(!is_valid_element_name("1a"));
        assert!(!is_valid_element_name("a b"));
        assert!(is_valid_attribute_name("data-x"));
        assert!(!is_valid_attribute_name("a=b"));
        assert!(validate_and_extract(None, "svg:rect", true).is_err());
        assert!(validate_and_extract(Some(XML_NS.into()), "xml:lang", false).is_ok());
        assert!(validate_and_extract(Some("urn:x".into()), "xmlns", false).is_err());
    }

    #[test]
    fn rendered_text_breaks_lines_at_blocks() {
        let dom = catpaw_dom::parse_html(
            "<body><h1>Title</h1><p>One  two\n three</p><div>a<br>b</div><script>x()</script><span>tail</span>",
            &Default::default(),
        )
        .dom;
        let body = dom
            .descendants(dom.document())
            .find(|&n| dom.is_html_element(n, "body"))
            .unwrap();
        assert_eq!(
            rendered_text(&dom, body),
            "Title\n\nOne two three\n\na\nb\ntail"
        );
    }
}
