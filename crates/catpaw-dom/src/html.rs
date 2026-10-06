//! HTML parsing: an html5ever [`TreeSink`] over the arena.
//!
//! The tree builder calls the sink through `&self`, so the arena sits behind
//! a `RefCell` for the duration of a parse and is handed back by
//! [`TreeSink::finish`]. Streaming parses (M1: parser tasks interleaved with
//! script execution) will drive the same sink chunk by chunk.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{ParseOpts, parse_document, parse_fragment};
use markup5ever::interface::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use markup5ever::{Attribute, LocalName, Namespace, QualName, ns};
use url::Url;

use crate::arena::{Attr, DoctypeData, Dom, FragmentKind, NodeId, NodeKind};

/// Options for [`parse_html`].
#[derive(Debug, Clone)]
pub struct HtmlParseOptions {
    /// Whether `<noscript>` content is parsed as raw text (scripting on) or
    /// as markup (scripting off). Browsers with JavaScript enabled use `true`.
    pub scripting_enabled: bool,
    /// Parse as an `<iframe srcdoc>` document.
    pub iframe_srcdoc: bool,
    /// Document URL, recorded on the Document node.
    pub url: Option<Url>,
}

impl Default for HtmlParseOptions {
    fn default() -> Self {
        Self {
            scripting_enabled: true,
            iframe_srcdoc: false,
            url: None,
        }
    }
}

/// The outcome of a parse: the arena plus the parse errors html5ever reported.
#[derive(Debug)]
pub struct ParseResult {
    pub dom: Dom,
    pub errors: Vec<Cow<'static, str>>,
}

/// An owned element name, returned from [`TreeSink::elem_name`] because the
/// arena lives behind a `RefCell` and cannot hand out borrows.
pub struct OwnedElemName {
    ns: Namespace,
    local: LocalName,
}

impl fmt::Debug for OwnedElemName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{{}}}{}", &*self.ns, &*self.local)
    }
}

impl ElemName for OwnedElemName {
    fn ns(&self) -> &Namespace {
        &self.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.local
    }
}

/// The html5ever tree sink.
pub struct Sink {
    dom: RefCell<Dom>,
    errors: RefCell<Vec<Cow<'static, str>>>,
    current_line: Cell<u64>,
}

impl Sink {
    pub fn new(dom: Dom) -> Self {
        Self {
            dom: RefCell::new(dom),
            errors: RefCell::new(Vec::new()),
            current_line: Cell::new(1),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Dom) -> R) -> R {
        f(&mut self.dom.borrow_mut())
    }
}

fn append_text(dom: &mut Dom, parent: NodeId, text: &str) {
    if let Some(last) = dom.last_child(parent)
        && let NodeKind::Text(s) = &mut dom.node_mut(last).kind
    {
        s.push_str(text);
        return;
    }
    let n = dom.create_text(text);
    dom.append_child(parent, n);
}

fn convert_attrs(attrs: Vec<Attribute>) -> Vec<Attr> {
    attrs
        .into_iter()
        .map(|a| Attr::new(a.name, String::from(&*a.value)))
        .collect()
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = ParseResult;
    type ElemName<'a>
        = OwnedElemName
    where
        Self: 'a;

    fn finish(self) -> ParseResult {
        ParseResult {
            dom: self.dom.into_inner(),
            errors: self.errors.into_inner(),
        }
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        self.errors.borrow_mut().push(msg);
    }

    fn get_document(&self) -> NodeId {
        self.dom.borrow().document()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> OwnedElemName {
        let dom = self.dom.borrow();
        let el = dom
            .element(*target)
            .expect("elem_name called on a non-element node");
        OwnedElemName {
            ns: el.name.ns.clone(),
            local: el.name.local.clone(),
        }
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        self.with(|dom| {
            let id = dom.create_element(name, convert_attrs(attrs));
            if flags.template {
                let contents = dom.create_fragment(FragmentKind::TemplateContents { host: id });
                dom.element_mut(id).unwrap().template_contents = Some(contents);
            }
            if flags.mathml_annotation_xml_integration_point {
                dom.element_mut(id)
                    .unwrap()
                    .mathml_annotation_xml_integration_point = true;
            }
            id
        })
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.with(|dom| dom.create_comment(&*text))
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.with(|dom| {
            dom.create(NodeKind::ProcessingInstruction {
                target: String::from(&*target),
                data: String::from(&*data),
            })
        })
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.with(|dom| match child {
            NodeOrText::AppendNode(n) => dom.append_child(*parent, n),
            NodeOrText::AppendText(t) => append_text(dom, *parent, &t),
        })
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self.dom.borrow().parent(*element).is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        self.with(|dom| {
            let doc = dom.document();
            let n = dom.create(NodeKind::Doctype(DoctypeData {
                name: String::from(&*name),
                public_id: String::from(&*public_id),
                system_id: String::from(&*system_id),
            }));
            dom.append_child(doc, n);
        });
    }

    fn mark_script_already_started(&self, node: &NodeId) {
        self.with(|dom| {
            if let Some(el) = dom.element_mut(*node) {
                el.script_already_started = true;
            }
        });
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.dom
            .borrow()
            .element(*target)
            .and_then(|e| e.template_contents)
            .expect("get_template_contents called on a non-template element")
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.with(|dom| dom.document_data_mut().quirks_mode = mode);
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        self.with(|dom| {
            let parent = dom
                .parent(*sibling)
                .expect("append_before_sibling: sibling has no parent");
            match new_node {
                NodeOrText::AppendNode(n) => dom.insert_before(parent, n, Some(*sibling)),
                NodeOrText::AppendText(t) => {
                    if let Some(prev) = dom.prev_sibling(*sibling)
                        && let NodeKind::Text(s) = &mut dom.node_mut(prev).kind
                    {
                        s.push_str(&t);
                        return;
                    }
                    let n = dom.create_text(&*t);
                    dom.insert_before(parent, n, Some(*sibling));
                }
            }
        });
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        self.with(|dom| {
            let el = dom
                .element_mut(*target)
                .expect("add_attrs_if_missing called on a non-element node");
            for a in attrs {
                if !el.attrs.iter().any(|existing| existing.name == a.name) {
                    el.attrs.push(Attr::new(a.name, &*a.value));
                }
            }
        });
    }

    fn associate_with_form(
        &self,
        target: &NodeId,
        form: &NodeId,
        _nodes: (&NodeId, Option<&NodeId>),
    ) {
        self.with(|dom| {
            if let Some(el) = dom.element_mut(*target) {
                el.form_owner = Some(*form);
            }
        });
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.with(|dom| dom.detach(*target));
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        self.with(|dom| dom.reparent_children(*node, *new_parent));
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool {
        self.dom
            .borrow()
            .element(*handle)
            .is_some_and(|e| e.mathml_annotation_xml_integration_point)
    }

    fn set_current_line(&self, line_number: u64) {
        self.current_line.set(line_number);
    }

    fn maybe_clone_an_option_into_selectedcontent(&self, option: &NodeId) {
        self.with(|dom| clone_option_into_selectedcontent(dom, *option));
    }

    // `attach_declarative_shadow` keeps the default (`false`) until shadow
    // trees land with the DOM APIs in M1; the `<template>` then stays in the
    // tree as an ordinary template element.
}

/// <https://html.spec.whatwg.org/multipage/#maybe-clone-an-option-into-selectedcontent>
///
/// Called by the parser when an `<option>` is popped. If the option is its
/// `<select>`'s selected option and the select has a `<selectedcontent>`,
/// the selectedcontent's children are replaced by clones of the option's.
fn clone_option_into_selectedcontent(dom: &mut Dom, option: NodeId) {
    let Some(select) = dom
        .ancestors(option)
        .find(|&a| dom.is_html_element(a, "select"))
    else {
        return;
    };
    let options: Vec<NodeId> = dom
        .descendants(select)
        .filter(|&n| dom.is_html_element(n, "option"))
        .collect();
    let has_selected_attr = |o: NodeId| dom.element(o).is_some_and(|e| e.has_attr("selected"));
    let is_selected = has_selected_attr(option)
        || (options.first() == Some(&option) && !options.iter().any(|&o| has_selected_attr(o)));
    if !is_selected {
        return;
    }
    let Some(selectedcontent) = dom
        .descendants(select)
        .find(|&n| dom.is_html_element(n, "selectedcontent"))
    else {
        return;
    };
    let old: Vec<NodeId> = dom.children(selectedcontent).collect();
    for c in old {
        dom.remove_subtree(c);
    }
    let kids: Vec<NodeId> = dom.children(option).collect();
    for c in kids {
        let clone = dom.clone_subtree(c);
        dom.append_child(selectedcontent, clone);
    }
}

fn parse_opts(options: &HtmlParseOptions) -> ParseOpts {
    ParseOpts {
        tokenizer: Default::default(),
        tree_builder: TreeBuilderOpts {
            scripting_enabled: options.scripting_enabled,
            iframe_srcdoc: options.iframe_srcdoc,
            ..Default::default()
        },
    }
}

/// Parses a complete HTML document from a string.
pub fn parse_html(input: &str, options: &HtmlParseOptions) -> ParseResult {
    let sink = Sink::new(Dom::with_url(options.url.clone()));
    parse_document(sink, parse_opts(options)).one(StrTendril::from(input))
}

/// Parses a complete HTML document from bytes, decoding them as UTF-8 with
/// replacement characters. Encoding sniffing is layered on top by the fetch
/// pipeline.
pub fn parse_html_bytes(input: &[u8], options: &HtmlParseOptions) -> ParseResult {
    let sink = Sink::new(Dom::with_url(options.url.clone()));
    parse_document(sink, parse_opts(options))
        .from_utf8()
        .one(input)
}

/// Parses `input` as a fragment in the context of an element named by
/// `context` (`"div"`, `"svg path"`, `"math mi"`, `"template"`, ...), the way
/// `innerHTML` does. The resulting document holds a synthetic `<html>` root
/// whose children are the parsed nodes.
pub fn parse_html_fragment(input: &str, context: &str, options: &HtmlParseOptions) -> ParseResult {
    let (ns, local) = match context.split_once(' ') {
        Some(("svg", local)) => (ns!(svg), local),
        Some(("math", local)) => (ns!(mathml), local),
        Some((_, local)) => (ns!(html), local),
        None => (ns!(html), context),
    };
    let context_name = QualName::new(None, ns, LocalName::from(local));
    let sink = Sink::new(Dom::with_url(options.url.clone()));
    parse_fragment(
        sink,
        parse_opts(options),
        context_name,
        Vec::new(),
        options.scripting_enabled,
    )
    .one(StrTendril::from(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialize::{html5lib_dump, to_html};

    #[test]
    fn explicit_option_end_tag_clones_into_selectedcontent() {
        // html5ever only runs the hook on an explicit </option>; implicitly
        // closed options are an upstream gap (see tests/tree-construction-expectations.txt).
        let r = parse(
            "<select><button><selectedcontent></selectedcontent></button><option>X<i>i</i></option><option selected>Y</option></select>",
        );
        let dom = &r.dom;
        let sc = dom
            .descendants(dom.document())
            .find(|&n| dom.is_html_element(n, "selectedcontent"))
            .unwrap();
        assert_eq!(dom.text_content(sc), "Y");
        assert_eq!(to_html(dom, sc, true), "Y");
    }

    #[test]
    fn parses_fragments_in_context() {
        let r = parse_html_fragment("<td>x", "tr", &HtmlParseOptions::default());
        let root = r.dom.first_child(r.dom.document()).unwrap();
        assert_eq!(html5lib_dump(&r.dom, root), "| <td>\n|   \"x\"\n");
    }

    fn parse(s: &str) -> ParseResult {
        parse_html(s, &HtmlParseOptions::default())
    }

    #[test]
    fn builds_a_simple_tree() {
        let r = parse("<!DOCTYPE html><p class=a>Hello <b>world</b></p>");
        let dump = html5lib_dump(&r.dom, r.dom.document());
        assert_eq!(
            dump,
            "| <!DOCTYPE html>\n\
             | <html>\n\
             |   <head>\n\
             |   <body>\n\
             |     <p>\n\
             |       class=\"a\"\n\
             |       \"Hello \"\n\
             |       <b>\n\
             |         \"world\"\n"
        );
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.dom.quirks_mode(), QuirksMode::NoQuirks);
    }

    #[test]
    fn missing_doctype_triggers_quirks_mode() {
        let r = parse("<p>x");
        assert_eq!(r.dom.quirks_mode(), QuirksMode::Quirks);
        assert!(!r.errors.is_empty());
    }

    #[test]
    fn adjacent_text_is_merged_and_foster_parenting_works() {
        let r = parse("<table>a<tr>b</table>");
        let dump = html5lib_dump(&r.dom, r.dom.document());
        assert_eq!(
            dump,
            "| <html>\n\
             |   <head>\n\
             |   <body>\n\
             |     \"ab\"\n\
             |     <table>\n\
             |       <tbody>\n\
             |         <tr>\n"
        );
    }

    #[test]
    fn template_contents_live_in_a_fragment() {
        let r = parse("<template><p>t</p></template>");
        let dom = &r.dom;
        let tpl = dom
            .descendants(dom.document())
            .find(|&n| dom.is_html_element(n, "template"))
            .unwrap();
        assert_eq!(dom.children(tpl).count(), 0);
        let contents = dom.element(tpl).unwrap().template_contents.unwrap();
        assert!(matches!(
            dom.kind(contents),
            NodeKind::DocumentFragment(FragmentKind::TemplateContents { .. })
        ));
        assert_eq!(dom.text_content(contents), "t");
        assert_eq!(to_html(dom, tpl, false), "<template><p>t</p></template>");
    }

    #[test]
    fn serializes_back_to_html() {
        let r = parse("<!DOCTYPE html><p class=a>Hello <b>world</b><br></p>");
        assert_eq!(
            to_html(&r.dom, r.dom.document(), true),
            "<!DOCTYPE html><html><head></head><body><p class=\"a\">Hello <b>world</b><br></p></body></html>"
        );
    }

    #[test]
    fn bytes_are_decoded_lossily() {
        let r = parse_html_bytes(b"<p>caf\xc3\xa9 \xff</p>", &HtmlParseOptions::default());
        let body = r
            .dom
            .descendants(r.dom.document())
            .find(|&n| r.dom.is_html_element(n, "p"))
            .unwrap();
        assert_eq!(r.dom.text_content(body), "café \u{FFFD}");
    }
}
