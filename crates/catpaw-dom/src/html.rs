//! HTML parsing: an html5ever [`TreeSink`] over the arena.
//!
//! The tree builder calls the sink through `&self`, so the arena sits behind
//! a shared `RefCell`. One-shot parses ([`parse_html`]) own the arena and get
//! it back from [`TreeSink::finish`]. A page that runs scripts shares its
//! arena with the parser instead and drives it through [`HtmlStream`], which
//! pauses at every `</script>` so the script can run before parsing resumes.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use html5ever::driver::parse_fragment_for_element;
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tokenizer::{BufferQueue, Tokenizer};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
use html5ever::{ParseOpts, parse_document, parse_fragment};
use markup5ever::interface::{
    ElemName, ElementFlags, NodeOrText, QuirksMode, TokenizerResult, TreeSink,
};
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
    dom: Rc<RefCell<Dom>>,
    /// The node the tree builder treats as the document: the Document node,
    /// or a scratch fragment when parsing a fragment into a shared arena.
    document: NodeId,
    scratch: bool,
    errors: RefCell<Vec<Cow<'static, str>>>,
    current_line: Cell<u64>,
}

impl Sink {
    /// A sink that owns its arena.
    pub fn new(dom: Dom) -> Self {
        Self::shared(Rc::new(RefCell::new(dom)))
    }

    /// A sink that parses into an arena shared with the caller.
    pub fn shared(dom: Rc<RefCell<Dom>>) -> Self {
        let document = dom.borrow().document();
        Self::for_document(dom, document)
    }

    /// A sink that parses into `document`, one of the documents of an arena
    /// shared with the caller.
    pub fn for_document(dom: Rc<RefCell<Dom>>, document: NodeId) -> Self {
        Self {
            dom,
            document,
            scratch: false,
            errors: RefCell::new(Vec::new()),
            current_line: Cell::new(1),
        }
    }

    /// A sink whose "document" is `root`, a detached scratch node in a
    /// shared arena (fragment parsing).
    fn scratch(dom: Rc<RefCell<Dom>>, root: NodeId) -> Self {
        Self {
            dom,
            document: root,
            scratch: true,
            errors: RefCell::new(Vec::new()),
            current_line: Cell::new(1),
        }
    }

    /// The line of the token being processed (1-based).
    pub fn current_line(&self) -> u64 {
        self.current_line.get()
    }

    pub fn take_errors(&self) -> Vec<Cow<'static, str>> {
        std::mem::take(&mut *self.errors.borrow_mut())
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
        // A shared arena stays with its other owners; the caller of a shared
        // parse ignores the (empty) arena returned here.
        let dom = match Rc::try_unwrap(self.dom) {
            Ok(cell) => cell.into_inner(),
            Err(_) => Dom::new(),
        };
        ParseResult {
            dom,
            errors: self.errors.into_inner(),
        }
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        self.errors.borrow_mut().push(msg);
    }

    fn get_document(&self) -> NodeId {
        self.document
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
        let doc = self.document;
        self.with(|dom| {
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
        if !self.scratch {
            self.with(|dom| {
                if let Some(data) = dom.document_data_of_mut(self.document) {
                    data.quirks_mode = mode;
                }
            });
        }
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

/// Parses `input` as a fragment directly into a shared arena, with the
/// element `context` as the context element (the `innerHTML` algorithm).
/// Returns a new detached `DocumentFragment` holding the parsed nodes.
pub fn parse_fragment_into(
    dom: &Rc<RefCell<Dom>>,
    input: &str,
    context: NodeId,
    scripting_enabled: bool,
) -> NodeId {
    let (root, form) = {
        let mut d = dom.borrow_mut();
        let form = std::iter::once(context)
            .chain(d.ancestors(context))
            .find(|&n| d.is_html_element(n, "form"));
        (d.create_fragment(FragmentKind::Plain), form)
    };
    let options = HtmlParseOptions {
        scripting_enabled,
        ..HtmlParseOptions::default()
    };
    let sink = Sink::scratch(dom.clone(), root);
    let _ =
        parse_fragment_for_element(sink, parse_opts(&options), context, scripting_enabled, form)
            .one(StrTendril::from(input));

    // The tree builder put the nodes under a synthetic <html> root.
    let mut d = dom.borrow_mut();
    if let Some(html) = d.first_child(root) {
        d.reparent_children(html, root);
        d.remove_subtree(html);
    }
    root
}

/// Parses `input` as a complete HTML document into `document`, a document
/// created in the shared arena for the purpose (see
/// [`Dom::create_document`]). Scripts are parsed, not run.
pub fn parse_document_into(
    dom: &Rc<RefCell<Dom>>,
    document: NodeId,
    input: &str,
    options: &HtmlParseOptions,
) {
    let sink = Sink::for_document(dom.clone(), document);
    let _ = parse_document(sink, parse_opts(options)).one(StrTendril::from(input));
    dom.borrow_mut().adopt_subtree(document, document);
}

/// Parses `input` as XML into `document`, a document created in the shared
/// arena for the purpose. Returns the errors the parser reported; it is
/// lenient, and builds a tree whatever the input.
pub fn parse_xml_into(
    dom: &Rc<RefCell<Dom>>,
    document: NodeId,
    input: &str,
) -> Vec<Cow<'static, str>> {
    let sink = Sink::for_document(dom.clone(), document);
    let result =
        xml5ever::driver::parse_document(sink, Default::default()).one(StrTendril::from(input));
    dom.borrow_mut().adopt_subtree(document, document);
    result.errors
}

/// Text waiting to be parsed at the insertion point (`document.write`).
pub struct WriteQueue(BufferQueue);

impl WriteQueue {
    pub fn new(text: &str) -> Self {
        let queue = BufferQueue::default();
        if !text.is_empty() {
            queue.push_back(StrTendril::from(text));
        }
        Self(queue)
    }
}

/// A document parse that pauses whenever a script is ready to run.
///
/// The caller owns the loop: [`HtmlStream::push`] source text, then call
/// [`HtmlStream::pump`] until it returns `None`, executing each returned
/// `<script>` element in between. Scripts may mutate the shared arena freely
/// while the stream is paused.
pub struct HtmlStream {
    tokenizer: Tokenizer<TreeBuilder<NodeId, Sink>>,
    input: BufferQueue,
    finished: Cell<bool>,
}

impl HtmlStream {
    pub fn new(dom: Rc<RefCell<Dom>>, options: &HtmlParseOptions) -> Self {
        let opts = parse_opts(options);
        let tree_builder = TreeBuilder::new(Sink::shared(dom), opts.tree_builder);
        Self {
            tokenizer: Tokenizer::new(tree_builder, opts.tokenizer),
            input: BufferQueue::default(),
            finished: Cell::new(false),
        }
    }

    /// Appends source text (from the network).
    pub fn push(&self, text: &str) {
        if !text.is_empty() {
            self.input.push_back(StrTendril::from(text));
        }
    }

    /// Parses buffered input until a `<script>` element is complete (returned
    /// so the caller can run it) or the input is exhausted (`None`).
    pub fn pump(&self) -> Option<NodeId> {
        self.feed(&self.input)
    }

    /// Like [`HtmlStream::pump`], for text inserted by `document.write`: it
    /// is tokenized ahead of the remaining network input.
    pub fn pump_write(&self, queue: &WriteQueue) -> Option<NodeId> {
        self.feed(&queue.0)
    }

    fn feed(&self, queue: &BufferQueue) -> Option<NodeId> {
        if self.finished.get() {
            return None;
        }
        loop {
            match self.tokenizer.feed(queue) {
                TokenizerResult::Script(node) => return Some(node),
                TokenizerResult::Done => return None,
                // A `<meta charset>`: the input was decoded before it got
                // here, so there is nothing to switch.
                TokenizerResult::EncodingIndicator(_) => {}
            }
        }
    }

    /// Signals the end of the input.
    pub fn finish(&self) {
        if !self.finished.replace(true) {
            self.tokenizer.end();
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished.get()
    }

    /// The source line of the token being processed (1-based).
    pub fn current_line(&self) -> u64 {
        self.tokenizer.sink.sink.current_line()
    }

    pub fn take_errors(&self) -> Vec<Cow<'static, str>> {
        self.tokenizer.sink.sink.take_errors()
    }
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

    #[test]
    fn stream_pauses_at_scripts_and_accepts_written_text() {
        let dom = Rc::new(RefCell::new(Dom::new()));
        let stream = HtmlStream::new(dom.clone(), &HtmlParseOptions::default());
        stream.push("<!DOCTYPE html><p>a</p><script>one()</scr");
        assert_eq!(stream.pump(), None, "the script is not complete yet");
        stream.push("ipt><p>b</p><script>two()</script><p>c</p>");

        let first = stream.pump().expect("first script");
        assert_eq!(dom.borrow().text_content(first), "one()");
        // Paused right after </script>: the following markup is not in the tree yet.
        let count = |local: &str| {
            let d = dom.borrow();
            d.descendants(d.document())
                .filter(|&n| d.is_html_element(n, local))
                .count()
        };
        assert_eq!(count("p"), 1);

        // document.write: text goes in ahead of the remaining input, and may
        // itself contain a script.
        let written = WriteQueue::new("<i>w</i><script>inner()</script><b>x</b>");
        let inner = stream.pump_write(&written).expect("written script");
        assert_eq!(dom.borrow().text_content(inner), "inner()");
        assert_eq!(stream.pump_write(&written), None);
        assert_eq!(count("b"), 1);
        assert_eq!(count("p"), 1);

        let second = stream.pump().expect("second script");
        assert_eq!(dom.borrow().text_content(second), "two()");
        assert_eq!(stream.pump(), None);
        stream.finish();
        assert!(stream.is_finished());

        let d = dom.borrow();
        let body = d
            .descendants(d.document())
            .find(|&n| d.is_html_element(n, "body"))
            .unwrap();
        assert_eq!(
            to_html(&d, body, true),
            "<p>a</p><script>one()</script><i>w</i><script>inner()</script><b>x</b><p>b</p><script>two()</script><p>c</p>"
        );
    }

    #[test]
    fn fragments_parse_into_a_shared_arena() {
        let dom = Rc::new(RefCell::new(
            parse("<table><tbody id=t></tbody></table><div id=d></div>").dom,
        ));
        let find = |id: &str| {
            let d = dom.borrow();
            d.descendants(d.document())
                .find(|&n| d.attr(n, "id") == Some(id))
                .unwrap()
        };
        let (tbody, div) = (find("t"), find("d"));
        let before = dom.borrow().len();

        let rows = parse_fragment_into(&dom, "<tr><td>1</td></tr><tr><td>2</td></tr>", tbody, true);
        {
            let d = dom.borrow();
            assert!(matches!(
                d.kind(rows),
                NodeKind::DocumentFragment(FragmentKind::Plain)
            ));
            assert_eq!(d.parent(rows), None);
            assert_eq!(
                to_html(&d, rows, true),
                "<tr><td>1</td></tr><tr><td>2</td></tr>"
            );
            // Only the fragment and the parsed nodes were added.
            assert_eq!(d.len(), before + 1 + 6);
            // The document itself is untouched.
            assert_eq!(d.child_elements(d.document()).count(), 1);
        }

        // In a div context, table rows are not allowed and collapse to text.
        let text = parse_fragment_into(&dom, "<tr><td>x</td></tr><b>y</b>", div, true);
        assert_eq!(to_html(&dom.borrow(), text, true), "x<b>y</b>");
    }
}

#[cfg(test)]
mod other_document_tests {
    use super::*;
    use crate::DocumentData;

    fn find(dom: &Dom, root: NodeId, local: &str) -> NodeId {
        dom.descendants(root)
            .find(|&n| dom.element(n).is_some_and(|e| &*e.name.local == local))
            .unwrap_or_else(|| panic!("no <{local}>"))
    }

    #[test]
    fn other_documents_live_in_the_same_arena() {
        let dom = Rc::new(RefCell::new(Dom::new()));
        let main = dom.borrow().document();
        let (page, data) = {
            let mut d = dom.borrow_mut();
            let xml = DocumentData {
                is_xml: true,
                ..DocumentData::default()
            };
            (
                d.create_document(DocumentData::default()),
                d.create_document(xml),
            )
        };
        parse_document_into(
            &dom,
            page,
            "<title>t</title><p id=a>x</p><template><b></b></template>",
            &HtmlParseOptions::default(),
        );
        let errors = parse_xml_into(
            &dom,
            data,
            r#"<root xmlns:x="urn:x" a="1"><x:Item>t &amp; u<![CDATA[<c>]]></x:Item><empty/></root>"#,
        );
        assert!(errors.is_empty(), "{errors:?}");

        let mut d = dom.borrow_mut();
        // The arena's own document is untouched.
        assert!(!d.has_children(main));
        assert_eq!(d.quirks_mode(), QuirksMode::NoQuirks);
        assert_eq!(
            d.document_data_of(page).unwrap().quirks_mode,
            QuirksMode::Quirks
        );
        assert!(d.document_data_of(find(&d, page, "p")).is_none());

        // Nodes belong to the document they were parsed into.
        let p = find(&d, page, "p");
        assert_eq!(d.owner_document(p), page);
        assert_eq!(d.owner_document(page), page);
        assert!(d.in_document_tree(p) && !d.is_connected(p));
        let template = find(&d, page, "template");
        let contents = d.element(template).unwrap().template_contents.unwrap();
        assert_eq!(d.owner_document(contents), page);
        assert_eq!(d.owner_document(d.first_child(contents).unwrap()), page);

        let item = find(&d, data, "Item");
        assert_eq!(&*d.element(item).unwrap().name.ns, "urn:x");
        assert_eq!(d.text_content(item), "t & u<c>");
        assert_eq!(d.owner_document(item), data);
        assert_eq!(d.child_elements(find(&d, data, "root")).count(), 2);

        // A new node belongs to the arena's document until it is adopted.
        let fresh = d.create_html_element("i", Vec::new());
        assert_eq!(d.owner_document(fresh), main);
        assert!(!d.in_document_tree(fresh));
        d.append_child(p, fresh);
        d.adopt_subtree(fresh, page);
        assert_eq!(d.owner_document(fresh), page);

        // Clones belong where their originals do; a cloned document is its
        // own, with everything in it.
        let copy = d.clone_subtree(p);
        assert_eq!(d.owner_document(copy), page);
        assert_eq!(d.owner_document(d.last_child(copy).unwrap()), page);
        let twin = d.clone_subtree(page);
        assert_eq!(d.owner_document(twin), twin);
        assert_eq!(d.owner_document(find(&d, twin, "p")), twin);
    }

    #[test]
    fn the_xml_parser_reports_what_is_not_well_formed() {
        let parse = |input: &str| {
            let dom = Rc::new(RefCell::new(Dom::new()));
            let document = dom.borrow_mut().create_document(DocumentData::default());
            let errors = parse_xml_into(&dom, document, input);
            let has_root = dom.borrow().child_elements(document).next().is_some();
            (errors, has_root)
        };
        assert_eq!(parse("<a><b/></a>"), (Vec::new(), true));
        assert!(!parse("<a><b></a>").0.is_empty());
        assert!(!parse("<a></a><b></b>").0.is_empty());
        assert!(!parse("just text").1);
        assert!(!parse("").1);
    }
}
