//! Making documents: `DOMImplementation` and `DOMParser`.
//!
//! The documents made here live in the page's arena next to its own
//! document. They have no window: nothing in them runs or loads.

use catpaw_dom::{
    DoctypeData, DocumentData, HtmlParseOptions, NodeId, NodeKind, parse_document_into,
    parse_xml_into,
};
use catpaw_js::{Exception, Fallible, ObjectId};

use crate::element::validate_and_extract;
use crate::generated::{self as web, DOMParserSupportedType};
use crate::page::Cx;
use crate::{Web, node, platform_object};

const HTML_NS: &str = "http://www.w3.org/1999/xhtml";
const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// The namespace of the element that reports an XML parse error.
const PARSER_ERROR_NS: &str = "http://www.mozilla.org/newlayout/xml/parsererror.xml";

/// `document.implementation`.
pub struct ImplementationObject {
    document: NodeId,
}
platform_object!(ImplementationObject, DOMImplementation);

pub struct ParserObject;
platform_object!(ParserObject, DOMParser);

/// The `implementation` object of `document`.
pub(crate) fn of(cx: &Cx<'_>, document: NodeId) -> ObjectId {
    cx.page.alloc(ImplementationObject { document })
}

fn new_document(cx: &Cx<'_>, data: DocumentData) -> NodeId {
    cx.dom_mut().create_document(data)
}

impl web::DOMImplementationImpl for Web {
    fn create_document_type(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
        public_id: String,
        system_id: String,
    ) -> Fallible<NodeId> {
        let invalid = |c: char| c.is_ascii_whitespace() || c == '\0' || c == '>';
        if name.contains(invalid) {
            return Err(Exception::invalid_character(format!(
                "'{name}' is not a valid doctype name"
            )));
        }
        let document = cx
            .page
            .with::<ImplementationObject, _>(this, |i| i.document)?;
        let mut dom = cx.dom_mut();
        let doctype = dom.create(NodeKind::Doctype(DoctypeData {
            name,
            public_id,
            system_id,
        }));
        dom.adopt_subtree(doctype, document);
        Ok(doctype)
    }

    fn create_document(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        namespace: Option<String>,
        qualified_name: String,
        doctype: Option<NodeId>,
    ) -> Fallible<NodeId> {
        let content_type = match namespace.as_deref() {
            Some(HTML_NS) => "application/xhtml+xml",
            Some(SVG_NS) => "image/svg+xml",
            _ => "application/xml",
        };
        let element = if qualified_name.is_empty() {
            None
        } else {
            let name = validate_and_extract(namespace, &qualified_name, true)?;
            Some(node::create_element_node(&mut cx.dom_mut(), name))
        };
        let document = new_document(
            cx,
            DocumentData {
                is_xml: true,
                content_type: Some(content_type.to_string()),
                ..DocumentData::default()
            },
        );
        if let Some(doctype) = doctype {
            node::append(cx, doctype, document)?;
        }
        if let Some(element) = element {
            node::append(cx, element, document)?;
        }
        Ok(document)
    }

    fn create_html_document(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        title: Option<String>,
    ) -> Fallible<NodeId> {
        let document = new_document(cx, DocumentData::default());
        let mut dom = cx.dom_mut();
        let doctype = dom.create(NodeKind::Doctype(DoctypeData {
            name: "html".to_string(),
            ..DoctypeData::default()
        }));
        dom.append_child(document, doctype);
        let html = dom.create_html_element("html", Vec::new());
        dom.append_child(document, html);
        let head = dom.create_html_element("head", Vec::new());
        dom.append_child(html, head);
        if let Some(title) = title {
            let element = dom.create_html_element("title", Vec::new());
            dom.append_child(head, element);
            let text = dom.create_text(title);
            dom.append_child(element, text);
        }
        let body = dom.create_html_element("body", Vec::new());
        dom.append_child(html, body);
        dom.adopt_subtree(document, document);
        Ok(document)
    }

    fn has_feature(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<bool> {
        Ok(true)
    }
}

/// Whether the XML parser's complaints mean the input was not well formed.
/// The parser is lenient, and some of what it reports (the internal subset
/// of a doctype, for one) is fine XML.
fn is_fatal(errors: &[std::borrow::Cow<'static, str>]) -> bool {
    errors
        .iter()
        .any(|e| e.contains("doesn't match tag") || e.contains("in end phase"))
}

impl web::DOMParserImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(ParserObject))
    }

    fn parse_from_string(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        string: String,
        type_: DOMParserSupportedType,
    ) -> Fallible<NodeId> {
        let url = Some(cx.page.url.borrow().clone());
        if type_ == DOMParserSupportedType::TextHtml {
            let document = new_document(
                cx,
                DocumentData {
                    url,
                    ..DocumentData::default()
                },
            );
            // Scripting is off in a document without a window: `noscript`
            // is markup, and scripts never run, wherever they end up.
            let options = HtmlParseOptions {
                scripting_enabled: false,
                ..HtmlParseOptions::default()
            };
            parse_document_into(&cx.page.dom, document, &string, &options);
            let mut dom = cx.dom_mut();
            let scripts: Vec<NodeId> = dom
                .descendants(document)
                .filter(|&n| dom.is_html_element(n, "script"))
                .collect();
            for script in scripts {
                if let Some(element) = dom.element_mut(script) {
                    element.script_already_started = true;
                }
            }
            return Ok(document);
        }

        let document = new_document(
            cx,
            DocumentData {
                url,
                is_xml: true,
                content_type: Some(type_.as_str().to_string()),
                ..DocumentData::default()
            },
        );
        let errors = parse_xml_into(&cx.page.dom, document, &string);
        let mut dom = cx.dom_mut();
        // The XML declaration is not a node.
        let declaration = dom.first_child(document).filter(|&first| {
            matches!(dom.kind(first), NodeKind::ProcessingInstruction { target, .. } if target == "xml")
        });
        if let Some(declaration) = declaration {
            dom.detach(declaration);
        }
        if is_fatal(&errors) || dom.child_elements(document).next().is_none() {
            // What is not well formed becomes a document that says so.
            let children: Vec<NodeId> = dom.children(document).collect();
            for child in children {
                dom.detach(child);
            }
            let name =
                validate_and_extract(Some(PARSER_ERROR_NS.to_string()), "parsererror", true)?;
            let report = node::create_element_node(&mut dom, name);
            let reason = errors.first().map_or("no root element", |e| &**e);
            let text = dom.create_text(format!("XML parsing error: {reason}"));
            dom.append_child(report, text);
            dom.append_child(document, report);
            dom.adopt_subtree(document, document);
        }
        Ok(document)
    }
}

pub struct XMLSerializerObject;
platform_object!(XMLSerializerObject, XMLSerializer);

impl web::XMLSerializerImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(XMLSerializerObject))
    }

    fn serialize_to_string(cx: &mut Cx<'_>, this: ObjectId, root: NodeId) -> Fallible<String> {
        cx.page.with::<XMLSerializerObject, _>(this, |_| ())?;
        node::check(cx, root)?;
        Ok(catpaw_dom::serialize::to_xml(&cx.dom(), root))
    }
}
