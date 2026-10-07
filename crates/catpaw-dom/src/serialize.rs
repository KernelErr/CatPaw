//! Serialization: HTML output through html5ever's serializer, and the
//! html5lib "tree dump" format used by the tree-construction tests.

use std::fmt::Write as _;
use std::io;

use html5ever::serialize::{Serialize, SerializeOpts, Serializer, TraversalScope, serialize};
use markup5ever::{Namespace, ns};

use crate::arena::{Dom, NodeId, NodeKind};

/// A node of a [`Dom`] viewed through html5ever's [`Serialize`] trait.
pub struct SerializableNode<'a> {
    pub dom: &'a Dom,
    pub id: NodeId,
}

impl Serialize for SerializableNode<'_> {
    fn serialize<S: Serializer>(
        &self,
        serializer: &mut S,
        traversal_scope: TraversalScope,
    ) -> io::Result<()> {
        serialize_node(self.dom, self.id, serializer, traversal_scope)
    }
}

fn serialize_children<S: Serializer>(dom: &Dom, parent: NodeId, s: &mut S) -> io::Result<()> {
    for c in dom.children(parent) {
        serialize_node(dom, c, s, TraversalScope::IncludeNode)?;
    }
    Ok(())
}

fn serialize_node<S: Serializer>(
    dom: &Dom,
    id: NodeId,
    s: &mut S,
    scope: TraversalScope,
) -> io::Result<()> {
    let include_node = matches!(scope, TraversalScope::IncludeNode);
    match &dom.node(id).kind {
        NodeKind::Element(el) => {
            if include_node {
                s.start_elem(
                    el.name.clone(),
                    el.attrs.iter().map(|a| (&a.name, a.value.as_str())),
                )?;
            }
            // A template's children are its contents fragment.
            serialize_children(dom, el.template_contents.unwrap_or(id), s)?;
            if include_node {
                s.end_elem(el.name.clone())?;
            }
            Ok(())
        }
        NodeKind::Document(_) | NodeKind::DocumentFragment(_) => serialize_children(dom, id, s),
        NodeKind::Doctype(d) if include_node => s.write_doctype(&d.name),
        NodeKind::Text(t) if include_node => s.write_text(t),
        NodeKind::Comment(t) if include_node => s.write_comment(t),
        NodeKind::ProcessingInstruction { target, data } if include_node => {
            s.write_processing_instruction(target, data)
        }
        // ChildrenOnly on a childless node kind: nothing to emit.
        _ => Ok(()),
    }
}

/// Serializes `id` to HTML. With `children_only`, the node's own start and
/// end tags are omitted (`innerHTML` semantics); otherwise `outerHTML`.
pub fn to_html(dom: &Dom, id: NodeId, children_only: bool) -> String {
    let traversal_scope = if children_only {
        TraversalScope::ChildrenOnly(dom.element(id).map(|e| e.name.clone()))
    } else {
        TraversalScope::IncludeNode
    };
    let mut buf = Vec::new();
    serialize(
        &mut buf,
        &SerializableNode { dom, id },
        SerializeOpts {
            scripting_enabled: true,
            traversal_scope,
            create_missing_parent: false,
        },
    )
    .expect("serializing into memory cannot fail");
    String::from_utf8(buf).expect("the HTML serializer emits UTF-8")
}

fn element_ns_prefix(ns: &Namespace) -> &'static str {
    if *ns == ns!(svg) {
        "svg "
    } else if *ns == ns!(mathml) {
        "math "
    } else {
        ""
    }
}

fn attr_ns_prefix(ns: &Namespace) -> &'static str {
    if *ns == ns!(xlink) {
        "xlink "
    } else if *ns == ns!(xml) {
        "xml "
    } else if *ns == ns!(xmlns) {
        "xmlns "
    } else {
        ""
    }
}

/// Renders the children of `root` in the html5lib tree-construction format:
///
/// ```text
/// | <!DOCTYPE html>
/// | <html>
/// |   <head>
/// |   <body>
/// |     "text"
/// ```
pub fn html5lib_dump(dom: &Dom, root: NodeId) -> String {
    let mut out = String::new();
    dump_children(dom, root, 0, &mut out);
    out
}

fn dump_children(dom: &Dom, parent: NodeId, depth: usize, out: &mut String) {
    for c in dom.children(parent) {
        dump_node(dom, c, depth, out);
    }
}

fn dump_node(dom: &Dom, id: NodeId, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match &dom.node(id).kind {
        NodeKind::Doctype(d) => {
            let _ = write!(out, "| {indent}<!DOCTYPE {}", d.name);
            if !d.public_id.is_empty() || !d.system_id.is_empty() {
                let _ = write!(out, " \"{}\" \"{}\"", d.public_id, d.system_id);
            }
            out.push_str(">\n");
        }
        NodeKind::Element(el) => {
            let _ = writeln!(
                out,
                "| {indent}<{}{}>",
                element_ns_prefix(&el.name.ns),
                el.name.local
            );
            let mut attrs: Vec<(String, &str)> = el
                .attrs
                .iter()
                .map(|a| {
                    (
                        format!("{}{}", attr_ns_prefix(&a.name.ns), a.name.local),
                        a.value.as_str(),
                    )
                })
                .collect();
            attrs.sort();
            for (name, value) in attrs {
                let _ = writeln!(out, "| {indent}  {name}=\"{value}\"");
            }
            if let Some(contents) = el.template_contents {
                let _ = writeln!(out, "| {indent}  content");
                dump_children(dom, contents, depth + 2, out);
            }
            dump_children(dom, id, depth + 1, out);
        }
        NodeKind::Text(t) => {
            let _ = writeln!(out, "| {indent}\"{t}\"");
        }
        NodeKind::Comment(t) => {
            let _ = writeln!(out, "| {indent}<!-- {t} -->");
        }
        NodeKind::ProcessingInstruction { target, data } => {
            let _ = writeln!(out, "| {indent}<?{target} {data}>");
        }
        NodeKind::Document(_) | NodeKind::DocumentFragment(_) => {
            dump_children(dom, id, depth, out);
        }
    }
}

// ------------------------------------------------------------------ XML

/// The HTML void elements, which the XML serialization writes as `<br />`.
const VOID_ELEMENTS: [&str; 16] = [
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source",
];

fn escape_xml(text: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            '\t' if attribute => out.push_str("&#9;"),
            '\n' if attribute => out.push_str("&#xA;"),
            '\r' if attribute => out.push_str("&#xD;"),
            _ => out.push(c),
        }
    }
    out
}

/// The XML serialization of a node, as `XMLSerializer.serializeToString()`
/// and `outerHTML` on XML documents give it
/// (<https://w3c.github.io/DOM-Parsing/#xml-serialization>), without the
/// well-formedness checks: HTML elements come out as XHTML, with the
/// namespace declared where it changes from the parent's.
pub fn to_xml(dom: &Dom, id: NodeId) -> String {
    let mut out = String::new();
    write_xml(dom, id, None, &mut out);
    out
}

fn write_xml(dom: &Dom, id: NodeId, parent_ns: Option<&Namespace>, out: &mut String) {
    match dom.kind(id) {
        NodeKind::Document(_) | NodeKind::DocumentFragment(_) => {
            for child in dom.children(id) {
                write_xml(dom, child, parent_ns, out);
            }
        }
        NodeKind::Doctype(d) => {
            out.push_str("<!DOCTYPE ");
            out.push_str(&d.name);
            if !d.public_id.is_empty() {
                let _ = write!(out, " PUBLIC \"{}\"", d.public_id);
                if !d.system_id.is_empty() {
                    let _ = write!(out, " \"{}\"", d.system_id);
                }
            } else if !d.system_id.is_empty() {
                let _ = write!(out, " SYSTEM \"{}\"", d.system_id);
            }
            out.push('>');
        }
        NodeKind::Text(t) => out.push_str(&escape_xml(t, false)),
        NodeKind::Comment(t) => {
            let _ = write!(out, "<!--{t}-->");
        }
        NodeKind::ProcessingInstruction { target, data } => {
            let _ = write!(out, "<?{target} {data}?>");
        }
        NodeKind::Element(el) => {
            let name = match &el.name.prefix {
                Some(p) => format!("{p}:{}", el.name.local),
                None => el.name.local.to_string(),
            };
            out.push('<');
            out.push_str(&name);
            let declares = parent_ns != Some(&el.name.ns)
                && el.name.prefix.is_none()
                && !el
                    .attrs
                    .iter()
                    .any(|a| a.name.ns == ns!(xmlns) && &*a.name.local == "xmlns");
            if declares {
                let _ = write!(out, " xmlns=\"{}\"", escape_xml(&el.name.ns, true));
            }
            for attr in &el.attrs {
                let attr_name = if attr.name.ns == ns!(xml) {
                    format!("xml:{}", attr.name.local)
                } else if attr.name.ns == ns!(xmlns) {
                    if &*attr.name.local == "xmlns" {
                        "xmlns".to_string()
                    } else {
                        format!("xmlns:{}", attr.name.local)
                    }
                } else if attr.name.ns == ns!(xlink) {
                    format!("xlink:{}", attr.name.local)
                } else {
                    match &attr.name.prefix {
                        Some(p) => format!("{p}:{}", attr.name.local),
                        None => attr.name.local.to_string(),
                    }
                };
                let _ = write!(out, " {attr_name}=\"{}\"", escape_xml(&attr.value, true));
            }
            let is_html = el.name.ns == ns!(html);
            let children: Vec<NodeId> = dom.children(id).collect();
            let template = el.template_contents;
            if children.is_empty() && template.is_none() {
                if is_html && VOID_ELEMENTS.contains(&&*el.name.local) {
                    out.push_str(" />");
                } else if is_html {
                    let _ = write!(out, "></{name}>");
                } else {
                    out.push_str("/>");
                }
                return;
            }
            out.push('>');
            match template {
                Some(contents) => write_xml(dom, contents, Some(&el.name.ns), out),
                None => {
                    for child in children {
                        write_xml(dom, child, Some(&el.name.ns), out);
                    }
                }
            }
            let _ = write!(out, "</{name}>");
        }
    }
}

#[cfg(test)]
mod xml_tests {
    use super::*;
    use crate::html::{HtmlParseOptions, parse_html};

    #[test]
    fn serialises_html_as_xhtml() {
        let dom = parse_html(
            "<!DOCTYPE html><p class=\"a&b\">x<br>&lt;y<svg><rect/></svg><!--c--><template><i></i></template></p>",
            &HtmlParseOptions::default(),
        )
        .dom;
        let body = dom
            .descendants(dom.document())
            .find(|&n| dom.element(n).is_some_and(|el| &*el.name.local == "body"))
            .unwrap();
        let p = dom.children(body).next().unwrap();
        assert_eq!(
            to_xml(&dom, p),
            "<p xmlns=\"http://www.w3.org/1999/xhtml\" class=\"a&amp;b\">x<br />&lt;y<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg><!--c--><template><i></i></template></p>"
        );
        assert!(to_xml(&dom, dom.document()).starts_with(
            "<!DOCTYPE html><html xmlns=\"http://www.w3.org/1999/xhtml\"><head></head><body>"
        ));
    }
}
