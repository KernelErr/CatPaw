//! The `Document` interface.

use catpaw_dom::{Dom, FragmentKind, LocalName, NodeId, NodeKind, QualName, QuirksMode, ns};
use catpaw_js::{Exception, Fallible, ObjectId, WindowRef};

use crate::collections::{self, ListSource};
use crate::element::{child_text_content, is_valid_element_name, validate_and_extract};
use crate::generated::{self as web, DocumentReadyState, DocumentVisibilityState, InterfaceId};
use crate::page::Cx;
use crate::{Web, events, node, scripting, window};

fn html_element(dom: &Dom) -> Option<NodeId> {
    dom.child_elements(dom.document())
        .next()
        .filter(|&e| dom.is_html_element(e, "html"))
}

pub(crate) fn head(dom: &Dom) -> Option<NodeId> {
    let html = html_element(dom)?;
    dom.child_elements(html)
        .find(|&c| dom.is_html_element(c, "head"))
}

pub(crate) fn body(dom: &Dom) -> Option<NodeId> {
    let html = html_element(dom)?;
    dom.child_elements(html)
        .find(|&c| dom.is_html_element(c, "body") || dom.is_html_element(c, "frameset"))
}

fn title_element(dom: &Dom) -> Option<NodeId> {
    dom.descendants(dom.document())
        .find(|&n| dom.is_html_element(n, "title"))
}

impl web::DocumentImpl for Web {
    fn url(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(cx.page.url.borrow().to_string())
    }

    fn document_uri(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(cx.page.url.borrow().to_string())
    }

    fn compat_mode(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(if cx.dom().quirks_mode() == QuirksMode::Quirks {
            "BackCompat"
        } else {
            "CSS1Compat"
        }
        .to_string())
    }

    fn character_set(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(cx.page.document_state.borrow().charset.clone())
    }

    fn charset(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(cx.page.document_state.borrow().charset.clone())
    }

    fn input_encoding(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(cx.page.document_state.borrow().charset.clone())
    }

    fn content_type(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(cx.page.document_state.borrow().content_type.clone())
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
        _this: NodeId,
        local_name: String,
        _options: web::StringOrElementCreationOptions,
    ) -> Fallible<NodeId> {
        if !is_valid_element_name(&local_name) {
            return Err(Exception::invalid_character(format!(
                "'{local_name}' is not a valid element name"
            )));
        }
        let name = QualName::new(
            None,
            ns!(html),
            LocalName::from(local_name.to_ascii_lowercase()),
        );
        Ok(node::create_element_node(&mut cx.dom_mut(), name))
    }

    fn create_element_ns(
        cx: &mut Cx<'_>,
        _this: NodeId,
        namespace: Option<String>,
        qualified_name: String,
        _options: web::StringOrElementCreationOptions,
    ) -> Fallible<NodeId> {
        let name = validate_and_extract(namespace, &qualified_name, true)?;
        Ok(node::create_element_node(&mut cx.dom_mut(), name))
    }

    fn create_document_fragment(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<NodeId> {
        Ok(cx.dom_mut().create_fragment(FragmentKind::Plain))
    }

    fn create_text_node(cx: &mut Cx<'_>, _this: NodeId, data: String) -> Fallible<NodeId> {
        Ok(cx.dom_mut().create_text(data))
    }

    fn create_comment(cx: &mut Cx<'_>, _this: NodeId, data: String) -> Fallible<NodeId> {
        Ok(cx.dom_mut().create_comment(data))
    }

    fn import_node(
        cx: &mut Cx<'_>,
        _this: NodeId,
        node: NodeId,
        options: web::BooleanOrImportNodeOptions,
    ) -> Fallible<NodeId> {
        let deep = match options {
            web::BooleanOrImportNodeOptions::Boolean(deep) => deep,
            web::BooleanOrImportNodeOptions::ImportNodeOptions(o) => !o.self_only,
        };
        node::clone_node(cx, node, deep)
    }

    fn adopt_node(cx: &mut Cx<'_>, _this: NodeId, node: NodeId) -> Fallible<NodeId> {
        node::check(cx, node)?;
        if matches!(cx.dom().kind(node), NodeKind::Document(_)) {
            return Err(Exception::not_supported("A document cannot be adopted"));
        }
        node::remove(cx, node);
        Ok(node)
    }

    fn create_event(cx: &mut Cx<'_>, _this: NodeId, interface: String) -> Fallible<ObjectId> {
        let iface = match interface.to_ascii_lowercase().as_str() {
            "event" | "events" | "htmlevents" | "svgevents" => InterfaceId::Event,
            "customevent" => InterfaceId::CustomEvent,
            _ => {
                return Err(Exception::not_supported(format!(
                    "The event interface '{interface}' is not supported"
                )));
            }
        };
        Ok(events::uninitialized_event(cx.page, iface))
    }

    fn location(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<Option<ObjectId>> {
        Ok(Some(window::location(cx)))
    }

    fn referrer(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        Ok(cx.page.document_state.borrow().referrer.clone())
    }

    fn cookie(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
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

    fn set_cookie(cx: &mut Cx<'_>, _this: NodeId, value: String) -> Fallible<()> {
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

    fn ready_state(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<DocumentReadyState> {
        Ok(cx.page.document_state.borrow().ready_state)
    }

    fn title(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<String> {
        let dom = cx.dom();
        let text = title_element(&dom)
            .map(|t| child_text_content(&dom, t))
            .unwrap_or_default();
        Ok(text.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
    }

    fn set_title(cx: &mut Cx<'_>, _this: NodeId, value: String) -> Fallible<()> {
        let existing = title_element(&cx.dom());
        let title = match existing {
            Some(t) => t,
            None => {
                let Some(head) = head(&cx.dom()) else {
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

    fn body(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(body(&cx.dom()))
    }

    fn set_body(cx: &mut Cx<'_>, _this: NodeId, value: Option<NodeId>) -> Fallible<()> {
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
            (body(&dom), dom.child_elements(dom.document()).next())
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

    fn head(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(head(&cx.dom()))
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

    fn current_script(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<Option<NodeId>> {
        Ok(cx.page.document_state.borrow().current_script)
    }

    fn open(
        cx: &mut Cx<'_>,
        this: NodeId,
        _unused1: Option<String>,
        _unused2: Option<String>,
    ) -> Fallible<NodeId> {
        scripting::document_open(cx);
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

    fn close(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<()> {
        scripting::document_close(cx);
        Ok(())
    }

    fn write(cx: &mut Cx<'_>, _this: NodeId, text: Vec<String>) -> Fallible<()> {
        scripting::document_write(cx, &text.concat());
        Ok(())
    }

    fn writeln(cx: &mut Cx<'_>, _this: NodeId, text: Vec<String>) -> Fallible<()> {
        let mut text = text.concat();
        text.push('\n');
        scripting::document_write(cx, &text);
        Ok(())
    }

    fn default_view(_cx: &mut Cx<'_>, _this: NodeId) -> Fallible<Option<WindowRef>> {
        Ok(Some(WindowRef))
    }

    fn has_focus(_cx: &mut Cx<'_>, _this: NodeId) -> Fallible<bool> {
        Ok(true)
    }

    fn hidden(_cx: &mut Cx<'_>, _this: NodeId) -> Fallible<bool> {
        Ok(false)
    }

    fn visibility_state(_cx: &mut Cx<'_>, _this: NodeId) -> Fallible<DocumentVisibilityState> {
        Ok(DocumentVisibilityState::Visible)
    }
}

impl web::DocumentOrShadowRootImpl for Web {
    fn active_element(cx: &mut Cx<'_>, _this: NodeId) -> Fallible<Option<NodeId>> {
        let focused = cx.page.document_state.borrow().focused;
        let dom = cx.dom();
        Ok(focused
            .filter(|&n| dom.contains(n) && dom.is_connected(n))
            .or_else(|| body(&dom)))
    }
}
