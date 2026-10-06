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
