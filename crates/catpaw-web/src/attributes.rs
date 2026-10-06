//! Attributes as objects: `Attr` and `NamedNodeMap`.
//!
//! Attributes are stored on their elements, not as nodes of their own. An
//! `Attr` is a view of one attribute, found again by name each time it is
//! used. The page remembers which object stands for which attribute, so
//! that asking twice gives the same object. Once its attribute is gone
//! from the element an `Attr` stands alone, with the value it last had.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use catpaw_dom::{Attr, Dom, NodeId};
use catpaw_js::{Exception, Fallible, ObjectId};

use crate::element::{self, attr_qualified_name, is_valid_attribute_name, validate_and_extract};
use crate::generated as web;
use crate::page::{Cx, PageState};
use crate::{Web, node, platform_object};

pub struct AttrObject {
    /// The element the attribute is on, as far as is known.
    element: Option<NodeId>,
    /// Empty for no namespace.
    namespace: String,
    prefix: Option<String>,
    local: String,
    /// The value when last seen: all there is once the attribute is on no
    /// element.
    value: String,
    /// The node document while on no element.
    document: NodeId,
}
platform_object!(AttrObject, Attr);

impl AttrObject {
    fn name(&self) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}:{}", self.local),
            None => self.local.clone(),
        }
    }
}

/// `element.attributes`.
pub struct AttributeMapObject {
    element: NodeId,
}
platform_object!(AttributeMapObject, NamedNodeMap);

/// Which object stands for which attribute: element, namespace, local name.
#[derive(Default)]
pub(crate) struct AttrObjects {
    known: RefCell<HashMap<(NodeId, String, String), ObjectId>>,
    /// The size at which objects that are gone are next weeded out.
    prune_at: Cell<usize>,
}

fn find<'a>(dom: &'a Dom, element: NodeId, namespace: &str, local: &str) -> Option<&'a Attr> {
    dom.element(element)?
        .attrs
        .iter()
        .find(|a| &*a.name.ns == namespace && &*a.name.local == local)
}

/// The attribute `qualified_name` names on `element`: lowercased first on
/// an HTML element of an HTML document.
fn find_by_name<'a>(dom: &'a Dom, element: NodeId, qualified_name: &str) -> Option<&'a Attr> {
    let data = dom.element(element)?;
    let lowered;
    let name = if data.is_html() && node::in_html_document(dom, element) {
        lowered = qualified_name.to_ascii_lowercase();
        &lowered
    } else {
        qualified_name
    };
    data.attrs.iter().find(|a| attr_qualified_name(a) == *name)
}

/// The object that stands for `attr`, an attribute of `element`.
pub(crate) fn object_for(page: &PageState, dom: &Dom, element: NodeId, attr: &Attr) -> ObjectId {
    let key = (
        element,
        attr.name.ns.to_string(),
        attr.name.local.to_string(),
    );
    let objects = &page.attrs;
    let known = objects.known.borrow().get(&key).copied();
    let current = known.filter(|&id| {
        page.try_with::<AttrObject, _>(id, |a| a.element == Some(element)) == Some(true)
    });
    if let Some(id) = current {
        return id;
    }
    let id = page.alloc(AttrObject {
        element: Some(element),
        namespace: key.1.clone(),
        prefix: attr.name.prefix.as_ref().map(|p| p.to_string()),
        local: key.2.clone(),
        value: attr.value.to_string(),
        document: dom.owner_document(element),
    });
    let mut known = objects.known.borrow_mut();
    if known.len() >= objects.prune_at.get() {
        known.retain(|_, id| page.interface_of(*id).is_some());
        objects.prune_at.set((known.len() * 2).max(256));
    }
    known.insert(key, id);
    id
}

/// What an `Attr` object says about itself, and where its attribute is.
struct Seen {
    /// The element that has the attribute now.
    element: Option<NodeId>,
    namespace: String,
    prefix: Option<String>,
    local: String,
    value: String,
    name: String,
    document: NodeId,
}

/// Looks at the attribute behind an `Attr`. If its element no longer has
/// it, the object is on its own from here on.
fn see(cx: &Cx<'_>, this: ObjectId) -> Fallible<Seen> {
    let mut seen = cx.page.with::<AttrObject, _>(this, |a| Seen {
        element: a.element,
        namespace: a.namespace.clone(),
        prefix: a.prefix.clone(),
        local: a.local.clone(),
        value: a.value.clone(),
        name: a.name(),
        document: a.document,
    })?;
    let Some(element) = seen.element else {
        return Ok(seen);
    };
    let dom = cx.dom();
    let live = dom
        .contains(element)
        .then(|| find(&dom, element, &seen.namespace, &seen.local))
        .flatten();
    match live {
        Some(attr) => {
            seen.value = attr.value.to_string();
            seen.document = dom.owner_document(element);
        }
        None => {
            seen.element = None;
            cx.page.with::<AttrObject, _>(this, |a| a.element = None)?;
        }
    }
    Ok(seen)
}

/// Writes the attribute on `element`, reporting the change as any other.
fn write(
    cx: &mut Cx<'_>,
    element: NodeId,
    namespace: &str,
    prefix: Option<&str>,
    local: &str,
    value: String,
) -> Fallible<()> {
    if namespace.is_empty() {
        return element::set_attr(cx, element, local, value);
    }
    let qualified = match prefix {
        Some(prefix) => format!("{prefix}:{local}"),
        None => local.to_string(),
    };
    <Web as web::ElementImpl>::set_attribute_ns(
        cx,
        element,
        Some(namespace.to_string()),
        qualified,
        value,
    )
}

fn erase(cx: &mut Cx<'_>, element: NodeId, namespace: &str, local: &str) -> Fallible<()> {
    if namespace.is_empty() {
        element::remove_attr(cx, element, local);
        return Ok(());
    }
    <Web as web::ElementImpl>::remove_attribute_ns(
        cx,
        element,
        Some(namespace.to_string()),
        local.to_string(),
    )
}

/// Takes the attribute behind `attr` off `element`, leaving the object on
/// its own with the value the attribute had.
fn detach(cx: &mut Cx<'_>, element: NodeId, attr: ObjectId) -> Fallible<()> {
    let seen = see(cx, attr)?;
    cx.page.with::<AttrObject, _>(attr, |a| {
        a.value = seen.value.clone();
        a.document = seen.document;
        a.element = None;
    })?;
    erase(cx, element, &seen.namespace, &seen.local)
}

/// `element.attributes`.
pub(crate) fn map(cx: &Cx<'_>, element: NodeId) -> ObjectId {
    cx.page.alloc(AttributeMapObject { element })
}

/// `getAttributeNode()`.
pub(crate) fn get(cx: &Cx<'_>, element: NodeId, qualified_name: &str) -> Option<ObjectId> {
    let dom = cx.dom();
    let attr = find_by_name(&dom, element, qualified_name)?;
    Some(object_for(cx.page, &dom, element, attr))
}

/// `getAttributeNodeNS()`.
pub(crate) fn get_ns(
    cx: &Cx<'_>,
    element: NodeId,
    namespace: Option<&str>,
    local: &str,
) -> Option<ObjectId> {
    let dom = cx.dom();
    let attr = find(&dom, element, namespace.unwrap_or_default(), local)?;
    Some(object_for(cx.page, &dom, element, attr))
}

/// <https://dom.spec.whatwg.org/#concept-element-attributes-set>: puts the
/// attribute `attr` stands for on `element`, and returns the object of the
/// attribute it replaces.
pub(crate) fn set(cx: &mut Cx<'_>, element: NodeId, attr: ObjectId) -> Fallible<Option<ObjectId>> {
    node::check(cx, element)?;
    let seen = see(cx, attr)?;
    if seen.element.is_some_and(|owner| owner != element) {
        return Err(Exception::in_use_attribute(
            "The attribute is in use by another element",
        ));
    }
    let old = get_ns(cx, element, Some(&seen.namespace), &seen.local);
    if old == Some(attr) {
        return Ok(old);
    }
    if let Some(old) = old {
        // The object of the attribute being replaced keeps its value.
        let value = see(cx, old)?.value;
        cx.page.with::<AttrObject, _>(old, |a| {
            a.value = value;
            a.element = None;
        })?;
    }
    write(
        cx,
        element,
        &seen.namespace,
        seen.prefix.as_deref(),
        &seen.local,
        seen.value,
    )?;
    cx.page
        .with::<AttrObject, _>(attr, |a| a.element = Some(element))?;
    cx.page
        .attrs
        .known
        .borrow_mut()
        .insert((element, seen.namespace, seen.local), attr);
    Ok(old)
}

/// `removeAttributeNode()`.
pub(crate) fn remove(cx: &mut Cx<'_>, element: NodeId, attr: ObjectId) -> Fallible<ObjectId> {
    if see(cx, attr)?.element != Some(element) {
        return Err(Exception::not_found(
            "The attribute is not one of the element's",
        ));
    }
    detach(cx, element, attr)?;
    Ok(attr)
}

/// `createAttribute()`.
pub(crate) fn create(cx: &Cx<'_>, document: NodeId, local_name: String) -> Fallible<ObjectId> {
    if !is_valid_attribute_name(&local_name) {
        return Err(Exception::invalid_character(format!(
            "'{local_name}' is not a valid attribute name"
        )));
    }
    let local = if crate::document::is_html_document(&cx.dom(), document) {
        local_name.to_ascii_lowercase()
    } else {
        local_name
    };
    Ok(cx.page.alloc(AttrObject {
        element: None,
        namespace: String::new(),
        prefix: None,
        local,
        value: String::new(),
        document,
    }))
}

/// `createAttributeNS()`.
pub(crate) fn create_ns(
    cx: &Cx<'_>,
    document: NodeId,
    namespace: Option<String>,
    qualified_name: &str,
) -> Fallible<ObjectId> {
    let name = validate_and_extract(namespace, qualified_name, false)?;
    Ok(cx.page.alloc(AttrObject {
        element: None,
        namespace: name.ns.to_string(),
        prefix: name.prefix.as_ref().map(|p| p.to_string()),
        local: name.local.to_string(),
        value: String::new(),
        document,
    }))
}

impl web::AttrImpl for Web {
    fn namespace_uri(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        let seen = see(cx, this)?;
        Ok((!seen.namespace.is_empty()).then_some(seen.namespace))
    }

    fn prefix(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        Ok(see(cx, this)?.prefix)
    }

    fn local_name(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Ok(see(cx, this)?.local)
    }

    fn name(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Ok(see(cx, this)?.name)
    }

    fn value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Ok(see(cx, this)?.value)
    }

    fn set_value(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let seen = see(cx, this)?;
        cx.page
            .with::<AttrObject, _>(this, |a| a.value = value.clone())?;
        match seen.element {
            Some(element) => write(
                cx,
                element,
                &seen.namespace,
                seen.prefix.as_deref(),
                &seen.local,
                value,
            ),
            None => Ok(()),
        }
    }

    fn owner_element(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        Ok(see(cx, this)?.element)
    }

    fn specified(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        see(cx, this).map(|_| true)
    }

    fn node_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        see(cx, this).map(|_| node::ATTRIBUTE_NODE)
    }

    fn node_name(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Self::name(cx, this)
    }

    fn node_value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        Self::value(cx, this).map(Some)
    }

    fn set_node_value(cx: &mut Cx<'_>, this: ObjectId, value: Option<String>) -> Fallible<()> {
        Self::set_value(cx, this, value.unwrap_or_default())
    }

    fn text_content(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        Self::value(cx, this).map(Some)
    }

    fn set_text_content(cx: &mut Cx<'_>, this: ObjectId, value: Option<String>) -> Fallible<()> {
        Self::set_value(cx, this, value.unwrap_or_default())
    }

    fn owner_document(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        Ok(Some(see(cx, this)?.document))
    }

    fn parent_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        see(cx, this).map(|_| None)
    }

    fn parent_element(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        see(cx, this).map(|_| None)
    }
}

fn element_of(cx: &Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
    cx.page
        .with::<AttributeMapObject, _>(this, |map| map.element)
}

impl web::NamedNodeMapImpl for Web {
    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        let element = element_of(cx, this)?;
        Ok(cx.dom().element(element).map_or(0, |e| e.attrs.len()) as u32)
    }

    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        let element = element_of(cx, this)?;
        let dom = cx.dom();
        let attr = dom
            .element(element)
            .and_then(|e| e.attrs.get(index as usize));
        Ok(attr.map(|attr| object_for(cx.page, &dom, element, attr)))
    }

    fn get_named_item(
        cx: &mut Cx<'_>,
        this: ObjectId,
        qualified_name: String,
    ) -> Fallible<Option<ObjectId>> {
        let element = element_of(cx, this)?;
        Ok(get(cx, element, &qualified_name))
    }

    fn get_named_item_ns(
        cx: &mut Cx<'_>,
        this: ObjectId,
        namespace: Option<String>,
        local_name: String,
    ) -> Fallible<Option<ObjectId>> {
        let element = element_of(cx, this)?;
        Ok(get_ns(cx, element, namespace.as_deref(), &local_name))
    }

    fn set_named_item(
        cx: &mut Cx<'_>,
        this: ObjectId,
        attr: ObjectId,
    ) -> Fallible<Option<ObjectId>> {
        let element = element_of(cx, this)?;
        set(cx, element, attr)
    }

    fn set_named_item_ns(
        cx: &mut Cx<'_>,
        this: ObjectId,
        attr: ObjectId,
    ) -> Fallible<Option<ObjectId>> {
        let element = element_of(cx, this)?;
        set(cx, element, attr)
    }

    fn remove_named_item(
        cx: &mut Cx<'_>,
        this: ObjectId,
        qualified_name: String,
    ) -> Fallible<ObjectId> {
        let element = element_of(cx, this)?;
        let attr = get(cx, element, &qualified_name).ok_or_else(|| {
            Exception::not_found(format!("The element has no attribute '{qualified_name}'"))
        })?;
        detach(cx, element, attr)?;
        Ok(attr)
    }

    fn remove_named_item_ns(
        cx: &mut Cx<'_>,
        this: ObjectId,
        namespace: Option<String>,
        local_name: String,
    ) -> Fallible<ObjectId> {
        let element = element_of(cx, this)?;
        let attr = get_ns(cx, element, namespace.as_deref(), &local_name).ok_or_else(|| {
            Exception::not_found(format!("The element has no attribute '{local_name}'"))
        })?;
        detach(cx, element, attr)?;
        Ok(attr)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        Self::item(cx, this, index)
    }

    fn named_get(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<Option<ObjectId>> {
        let element = element_of(cx, this)?;
        Ok(get(cx, element, name))
    }

    fn named_properties(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<String>> {
        let element = element_of(cx, this)?;
        let dom = cx.dom();
        let Some(data) = dom.element(element) else {
            return Ok(Vec::new());
        };
        // An HTML element's attributes are found by lowercase names only.
        let lowercase_only = data.is_html() && node::in_html_document(&dom, element);
        let mut names: Vec<String> = Vec::new();
        for attr in &data.attrs {
            let name = attr_qualified_name(attr);
            let hidden = lowercase_only && name.chars().any(|c| c.is_ascii_uppercase());
            if !hidden && !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }
}
