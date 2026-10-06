//! Custom elements (<https://html.spec.whatwg.org/multipage/custom-elements.html>):
//! the registry, upgrades, and the reactions that run lifecycle callbacks.
//!
//! Elements the parser makes are upgraded once a definition exists for
//! them, as are elements made from markup; `createElement()` and `new`
//! construct synchronously. Customized built-in elements (`is`) are
//! supported in the same way. Form-associated custom elements are not.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};

use catpaw_dom::{Attr, CustomElementState, Dom, LocalName, NodeId, QualName, ns};
use catpaw_js::{Callback, Exception, Fallible, ObjectId, PromiseRef, Value};

use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{Web, node, platform_object};

/// One `customElements.define()`.
pub struct Definition {
    name: String,
    local_name: String,
    pub(crate) constructor: Callback,
    observed_attributes: Vec<String>,
    connected: Option<Callback>,
    disconnected: Option<Callback>,
    adopted: Option<Callback>,
    attribute_changed: Option<Callback>,
    /// The elements being upgraded through this definition's constructor,
    /// innermost last; `None` once the constructor has claimed one.
    construction_stack: Vec<Option<NodeId>>,
}

impl Definition {
    pub fn is_autonomous(&self) -> bool {
        self.local_name == self.name
    }

    pub fn local_name(&self) -> &str {
        &self.local_name
    }
}

enum Reaction {
    Upgrade(u32),
    Callback {
        callback: Callback,
        args: Vec<Value>,
    },
}

/// The page's `CustomElementRegistry`.
#[derive(Default)]
pub(crate) struct Registry {
    definitions: RefCell<Vec<Definition>>,
    /// Promises from `whenDefined()` for names not defined yet.
    when_defined: RefCell<HashMap<String, Vec<PromiseRef>>>,
    /// A definition is being read off its constructor.
    defining: Cell<bool>,
    /// The custom element reactions stack: one element queue per
    /// `[CEReactions]` member being run.
    stack: RefCell<Vec<Vec<NodeId>>>,
    /// The backup element queue, for reactions enqueued outside any member.
    backup: RefCell<Vec<NodeId>>,
    backup_scheduled: Cell<bool>,
    /// Each element's own reaction queue.
    queues: RefCell<HashMap<NodeId, VecDeque<Reaction>>>,
}

pub struct RegistryObject;
platform_object!(RegistryObject, CustomElementRegistry);

/// <https://html.spec.whatwg.org/multipage/custom-elements.html#valid-custom-element-name>
pub(crate) fn is_valid_custom_element_name(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "annotation-xml",
        "color-profile",
        "font-face",
        "font-face-src",
        "font-face-uri",
        "font-face-format",
        "font-face-name",
        "missing-glyph",
    ];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || !name.contains('-') || RESERVED.contains(&name) {
        return false;
    }
    chars.all(|c| {
        matches!(c, '-' | '.' | '_' | '0'..='9' | 'a'..='z' | '\u{B7}')
            || matches!(c as u32,
                0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x37D | 0x37F..=0x1FFF | 0x200C..=0x200D
                | 0x203F..=0x2040 | 0x2070..=0x218F | 0x2C00..=0x2FEF | 0x3001..=0xD7FF
                | 0xF900..=0xFDCF | 0xFDF0..=0xFFFD | 0x10000..=0xEFFFF)
    })
}

/// The definition for an element with `local_name` and `is` value, by index.
fn lookup(page: &PageState, local_name: &str, is: Option<&str>) -> Option<u32> {
    let definitions = page.custom_elements.definitions.borrow();
    definitions
        .iter()
        .position(|d| d.local_name == local_name && d.name == is.unwrap_or(local_name))
        .map(|i| i as u32)
}

/// The definition an element in the tree would be upgraded by.
fn definition_for(page: &PageState, dom: &Dom, element: NodeId) -> Option<u32> {
    let data = dom.element(element)?;
    if !data.is_html() || data.custom_element_state != CustomElementState::Undefined {
        return None;
    }
    let is_attribute = data.attr("is").map(str::to_string);
    let is = data.is_value.as_deref().or(is_attribute.as_deref());
    lookup(page, &data.name.local, is)
}

pub fn with_definition<R>(
    page: &PageState,
    index: u32,
    f: impl FnOnce(&Definition) -> R,
) -> Option<R> {
    page.custom_elements
        .definitions
        .borrow()
        .get(index as usize)
        .map(f)
}

/// The constructors of every definition, with their indices.
pub fn constructors(page: &PageState) -> Vec<(u32, Callback)> {
    page.custom_elements
        .definitions
        .borrow()
        .iter()
        .enumerate()
        .map(|(i, d)| (i as u32, d.constructor.clone()))
        .collect()
}

// ---- reactions -------------------------------------------------------------

/// Opens a `[CEReactions]` scope: reactions enqueued inside it run when it
/// closes.
pub fn push_reactions(page: &PageState) {
    page.custom_elements.stack.borrow_mut().push(Vec::new());
}

/// Closes the innermost `[CEReactions]` scope and runs its reactions.
pub fn pop_reactions(cx: &mut Cx<'_>) {
    let queue = cx.page.custom_elements.stack.borrow_mut().pop();
    if let Some(queue) = queue {
        invoke(cx, queue);
    }
}

/// <https://html.spec.whatwg.org/multipage/custom-elements.html#enqueue-an-element-on-the-appropriate-element-queue>
fn enqueue_element(page: &PageState, element: NodeId) {
    let registry = &page.custom_elements;
    if let Some(queue) = registry.stack.borrow_mut().last_mut() {
        queue.push(element);
        return;
    }
    registry.backup.borrow_mut().push(element);
    if !registry.backup_scheduled.replace(true) {
        page.queue_microtask(|cx| {
            let registry = &cx.page.custom_elements;
            registry.backup_scheduled.set(false);
            let queue = std::mem::take(&mut *registry.backup.borrow_mut());
            invoke(cx, queue);
        });
    }
}

fn enqueue(page: &PageState, element: NodeId, reaction: Reaction) {
    page.custom_elements
        .queues
        .borrow_mut()
        .entry(element)
        .or_default()
        .push_back(reaction);
    enqueue_element(page, element);
}

/// Enqueues a lifecycle callback of the element's definition, if it has
/// one.
fn enqueue_callback(
    page: &PageState,
    element: NodeId,
    pick: impl FnOnce(&Definition) -> Option<Callback>,
    args: Vec<Value>,
) {
    let state = page
        .dom
        .borrow()
        .element(element)
        .map(|e| e.custom_element_state);
    let Some(CustomElementState::Custom(index)) = state else {
        return;
    };
    let Some(Some(callback)) = with_definition(page, index, pick) else {
        return;
    };
    enqueue(page, element, Reaction::Callback { callback, args });
}

/// Runs the reactions queued for each element of `queue`, in order.
fn invoke(cx: &mut Cx<'_>, queue: Vec<NodeId>) {
    for element in queue {
        loop {
            let next = cx
                .page
                .custom_elements
                .queues
                .borrow_mut()
                .get_mut(&element)
                .and_then(VecDeque::pop_front);
            let Some(reaction) = next else {
                cx.page.custom_elements.queues.borrow_mut().remove(&element);
                break;
            };
            let result = match reaction {
                Reaction::Upgrade(index) => upgrade(cx, element, index),
                Reaction::Callback { callback, args } => cx
                    .script
                    .call(&callback, &Value::Node(element), &args)
                    .map(drop),
            };
            if let Err(e) = result {
                cx.report_exception(&e);
            }
        }
    }
}

/// <https://html.spec.whatwg.org/multipage/custom-elements.html#concept-upgrade-an-element>
fn upgrade(cx: &mut Cx<'_>, element: NodeId, index: u32) -> Fallible<()> {
    let page = cx.page;
    let (attributes, connected) = {
        let mut dom = page.dom.borrow_mut();
        let connected = dom.is_connected(element);
        let Some(data) = dom.element_mut(element) else {
            return Ok(());
        };
        if data.custom_element_state != CustomElementState::Undefined {
            return Ok(());
        }
        // Failed until the constructor says otherwise.
        data.custom_element_state = CustomElementState::Failed;
        (data.attrs.clone(), connected)
    };
    let constructor = {
        let mut definitions = page.custom_elements.definitions.borrow_mut();
        let Some(definition) = definitions.get_mut(index as usize) else {
            return Ok(());
        };
        definition.construction_stack.push(Some(element));
        definition.constructor.clone()
    };
    // The callbacks for what the element already has run once it is
    // constructed; the constructor sees the element in place.
    for attribute in &attributes {
        attribute_changed_after(page, element, index, attribute, None);
    }
    if connected {
        enqueue_connected(page, element, index);
    }

    let result = cx.script.construct(&constructor, &[]);
    page.custom_elements
        .definitions
        .borrow_mut()
        .get_mut(index as usize)
        .map(|d| d.construction_stack.pop());
    let result = match result {
        Ok(Value::Node(constructed)) if constructed == element => Ok(()),
        Ok(_) => Err(Exception::type_error(
            "The custom element constructor did not return the element being upgraded",
        )),
        Err(e) => Err(e),
    };
    if result.is_err() {
        // What was queued for the element is dropped with it.
        page.custom_elements.queues.borrow_mut().remove(&element);
        return result;
    }
    if let Some(data) = page.dom.borrow_mut().element_mut(element) {
        data.custom_element_state = CustomElementState::Custom(index);
    }
    Ok(())
}

fn enqueue_connected(page: &PageState, element: NodeId, index: u32) {
    let callback = with_definition(page, index, |d| d.connected.clone()).flatten();
    if let Some(callback) = callback {
        enqueue(
            page,
            element,
            Reaction::Callback {
                callback,
                args: Vec::new(),
            },
        );
    }
}

/// Enqueues `attributeChangedCallback` for `attribute`, if it is observed.
fn attribute_changed_after(
    page: &PageState,
    element: NodeId,
    index: u32,
    attribute: &Attr,
    old: Option<&str>,
) {
    let callback = with_definition(page, index, |d| {
        let observed = d
            .observed_attributes
            .iter()
            .any(|name| name == &*attribute.name.local);
        observed.then(|| d.attribute_changed.clone()).flatten()
    })
    .flatten();
    let Some(callback) = callback else {
        return;
    };
    let namespace = (!attribute.name.ns.is_empty()).then(|| attribute.name.ns.to_string());
    let args = vec![
        Value::String(attribute.name.local.to_string()),
        old.map_or(Value::Null, |v| Value::String(v.to_string())),
        Value::String(attribute.value.to_string()),
        namespace.map_or(Value::Null, Value::String),
    ];
    enqueue(page, element, Reaction::Callback { callback, args });
}

// ---- hooks ----------------------------------------------------------------

/// Called when `element` is (or may be) a custom element in a tree that
/// was just inserted into the document, in tree order.
fn try_upgrade_or_connect(page: &PageState, dom: &Dom, element: NodeId) {
    let Some(data) = dom.element(element) else {
        return;
    };
    match data.custom_element_state {
        CustomElementState::Custom(index) => enqueue_connected(page, element, index),
        CustomElementState::Undefined => {
            if let Some(index) = definition_for(page, dom, element) {
                enqueue(page, element, Reaction::Upgrade(index));
            }
        }
        CustomElementState::Failed => {}
    }
}

/// Called after `nodes` were inserted into the document.
pub(crate) fn nodes_inserted(page: &PageState, nodes: &[NodeId]) {
    if page.custom_elements.definitions.borrow().is_empty() {
        return;
    }
    let dom = page.dom.borrow();
    for &node in nodes {
        for descendant in dom.traverse(node) {
            try_upgrade_or_connect(page, &dom, descendant);
        }
    }
}

/// Called before `node` is removed from the document.
pub(crate) fn subtree_removed(page: &PageState, node: NodeId) {
    if page.custom_elements.definitions.borrow().is_empty() {
        return;
    }
    let custom: Vec<NodeId> = {
        let dom = page.dom.borrow();
        dom.traverse(node)
            .filter(|&n| {
                dom.element(n).is_some_and(|e| {
                    matches!(e.custom_element_state, CustomElementState::Custom(_))
                })
            })
            .collect()
    };
    for element in custom {
        enqueue_callback(page, element, |d| d.disconnected.clone(), Vec::new());
    }
}

/// Called after `node` was adopted into another document.
pub(crate) fn subtree_adopted(page: &PageState, node: NodeId, old: NodeId, new: NodeId) {
    if page.custom_elements.definitions.borrow().is_empty() {
        return;
    }
    let custom: Vec<NodeId> = {
        let dom = page.dom.borrow();
        dom.traverse(node)
            .filter(|&n| {
                dom.element(n).is_some_and(|e| {
                    matches!(e.custom_element_state, CustomElementState::Custom(_))
                })
            })
            .collect()
    };
    for element in custom {
        enqueue_callback(
            page,
            element,
            |d| d.adopted.clone(),
            vec![Value::Node(old), Value::Node(new)],
        );
    }
}

/// Called after an attribute of `element` changed; `new` is `None` when
/// it was removed.
pub(crate) fn attribute_changed(
    page: &PageState,
    element: NodeId,
    local: &str,
    namespace: Option<&str>,
    old: Option<&str>,
    new: Option<&str>,
) {
    let state = page
        .dom
        .borrow()
        .element(element)
        .map(|e| e.custom_element_state);
    let Some(CustomElementState::Custom(index)) = state else {
        return;
    };
    let callback = with_definition(page, index, |d| {
        let observed = d.observed_attributes.iter().any(|name| name == local);
        observed.then(|| d.attribute_changed.clone()).flatten()
    })
    .flatten();
    let Some(callback) = callback else {
        return;
    };
    let args = vec![
        Value::String(local.to_string()),
        old.map_or(Value::Null, |v| Value::String(v.to_string())),
        new.map_or(Value::Null, |v| Value::String(v.to_string())),
        namespace.map_or(Value::Null, |ns| Value::String(ns.to_string())),
    ];
    enqueue(page, element, Reaction::Callback { callback, args });
}

/// Called with elements the parser inserted.
pub(crate) fn parser_inserted(page: &PageState, nodes: &[NodeId]) {
    if page.custom_elements.definitions.borrow().is_empty() {
        return;
    }
    let dom = page.dom.borrow();
    for &node in nodes {
        if dom.is_connected(node) {
            try_upgrade_or_connect(page, &dom, node);
        }
    }
}

/// Called with a freshly cloned or fragment-parsed subtree: elements with
/// a definition are upgraded, connected or not.
pub(crate) fn subtree_created(page: &PageState, root: NodeId) {
    if page.custom_elements.definitions.borrow().is_empty() {
        return;
    }
    let dom = page.dom.borrow();
    for element in dom.traverse(root) {
        if let Some(index) = definition_for(page, &dom, element) {
            enqueue(page, element, Reaction::Upgrade(index));
        }
    }
}

// ---- creation -------------------------------------------------------------

/// Makes the element a custom element constructor's `super()` call stands
/// for when there is no element being upgraded: a new one of the
/// definition's kind.
pub fn create_for_constructor(cx: &mut Cx<'_>, index: u32, document: NodeId) -> Option<NodeId> {
    let (local_name, is) = with_definition(cx.page, index, |d| {
        let is = (!d.is_autonomous()).then(|| d.name.clone());
        (d.local_name.clone(), is)
    })?;
    let mut dom = cx.dom_mut();
    let name = QualName::new(None, ns!(html), LocalName::from(local_name));
    let element = node::create_element_node(&mut dom, name);
    if let Some(data) = dom.element_mut(element) {
        data.custom_element_state = CustomElementState::Custom(index);
        data.is_value = is;
    }
    dom.adopt_subtree(element, document);
    Some(element)
}

/// The construction stack entry a `super()` call claims: the element
/// being upgraded, or `None` when the constructor is making a new one.
pub fn claim_construction(page: &PageState, index: u32) -> Result<Option<NodeId>, Exception> {
    let mut definitions = page.custom_elements.definitions.borrow_mut();
    let Some(definition) = definitions.get_mut(index as usize) else {
        return Ok(None);
    };
    match definition.construction_stack.last_mut() {
        None => Ok(None),
        Some(entry) => match entry.take() {
            Some(element) => Ok(Some(element)),
            None => Err(Exception::type_error(
                "The custom element constructor was called again for an element already constructed",
            )),
        },
    }
}

/// Creates an element of a defined kind the way `createElement()` does:
/// by running the constructor. A constructor that misbehaves leaves an
/// `HTMLUnknownElement` that failed to upgrade.
pub(crate) fn create_synchronously(
    cx: &mut Cx<'_>,
    document: NodeId,
    local_name: &str,
    is: Option<&str>,
) -> Option<NodeId> {
    let index = lookup(cx.page, local_name, is)?;
    let constructor = with_definition(cx.page, index, |d| d.constructor.clone())?;
    let result = cx.script.construct(&constructor, &[]);
    let checked = result.and_then(|value| {
        let Value::Node(element) = value else {
            return Err(Exception::type_error(
                "The custom element constructor did not return an element",
            ));
        };
        let dom = cx.dom();
        let data = dom
            .element(element)
            .ok_or_else(|| Exception::type_error("The constructor did not return an element"))?;
        let well_formed = data.is_html()
            && &*data.name.local == local_name
            && data.attrs.is_empty()
            && !dom.has_children(element)
            && dom.parent(element).is_none();
        if !well_formed {
            return Err(Exception::not_supported(
                "The custom element constructor returned an element that is not a fresh one of its kind",
            ));
        }
        Ok(element)
    });
    match checked {
        Ok(element) => {
            cx.dom_mut().adopt_subtree(element, document);
            Some(element)
        }
        Err(e) => {
            cx.report_exception(&e);
            let mut dom = cx.dom_mut();
            let name = QualName::new(None, ns!(html), LocalName::from(local_name));
            let element = node::create_element_node(&mut dom, name);
            if let Some(data) = dom.element_mut(element) {
                data.custom_element_state = CustomElementState::Failed;
                data.is_value = is.map(str::to_string);
            }
            dom.adopt_subtree(element, document);
            Some(element)
        }
    }
}

/// Whether a failed custom element is an `HTMLUnknownElement`.
pub(crate) fn interface_of_failed(dom: &Dom, element: NodeId) -> Option<InterfaceId> {
    let data = dom.element(element)?;
    (data.custom_element_state == CustomElementState::Failed
        && data.is_value.is_none()
        && data.is_html())
    .then_some(InterfaceId::HTMLUnknownElement)
}

// ---- the registry interface ------------------------------------------------

fn lifecycle(cx: &mut Cx<'_>, prototype: &Value, name: &str) -> Fallible<Option<Callback>> {
    let value = cx.script.get_property(prototype, name)?;
    if matches!(value, Value::Undefined) {
        return Ok(None);
    }
    cx.script.as_callback(&value).map(Some).ok_or_else(|| {
        Exception::type_error(format!(
            "The {name} of the custom element is not a function"
        ))
    })
}

impl web::CustomElementRegistryImpl for Web {
    fn define(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        name: String,
        constructor: Callback,
        options: web::ElementDefinitionOptions,
    ) -> Fallible<()> {
        let page = cx.page;
        if !cx.script.is_constructor(&constructor) {
            return Err(Exception::type_error(
                "The custom element constructor is not a constructor",
            ));
        }
        if !is_valid_custom_element_name(&name) {
            return Err(Exception::syntax(format!(
                "'{name}' is not a valid custom element name"
            )));
        }
        {
            let definitions = page.custom_elements.definitions.borrow();
            if definitions.iter().any(|d| d.name == name) {
                return Err(Exception::not_supported(format!(
                    "A custom element named '{name}' has already been defined"
                )));
            }
        }
        for (_, defined) in constructors(page) {
            if cx.script.same_callback(&defined, &constructor) {
                return Err(Exception::not_supported(
                    "The constructor has already been used to define a custom element",
                ));
            }
        }
        let local_name = match options.extends {
            None => name.clone(),
            Some(extends) => {
                if is_valid_custom_element_name(&extends) {
                    return Err(Exception::not_supported(format!(
                        "'{extends}' is itself a custom element name and cannot be extended"
                    )));
                }
                let known = InterfaceId::for_html_tag(&extends).is_some()
                    || crate::html_names::is_known_html_element(&extends);
                if !known {
                    return Err(Exception::not_supported(format!(
                        "'{extends}' is not an HTML element that can be extended"
                    )));
                }
                extends
            }
        };
        if page.custom_elements.defining.replace(true) {
            return Err(Exception::not_supported(
                "A custom element is already being defined",
            ));
        }

        let read = (|| -> Fallible<Definition> {
            let class = Value::Callback(constructor.clone());
            let prototype = cx.script.get_property(&class, "prototype")?;
            if !matches!(
                prototype,
                Value::Opaque(_) | Value::Object(_) | Value::Callback(_)
            ) {
                return Err(Exception::type_error(
                    "The custom element constructor's prototype is not an object",
                ));
            }
            let connected = lifecycle(cx, &prototype, "connectedCallback")?;
            let disconnected = lifecycle(cx, &prototype, "disconnectedCallback")?;
            let adopted = lifecycle(cx, &prototype, "adoptedCallback")?;
            let attribute_changed = lifecycle(cx, &prototype, "attributeChangedCallback")?;
            let mut observed_attributes = Vec::new();
            if attribute_changed.is_some() {
                let observed = cx.script.get_property(&class, "observedAttributes")?;
                if !matches!(observed, Value::Undefined) {
                    observed_attributes = cx.script.to_string_sequence(&observed)?;
                }
            }
            Ok(Definition {
                name: name.clone(),
                local_name: local_name.clone(),
                constructor: constructor.clone(),
                observed_attributes,
                connected,
                disconnected,
                adopted,
                attribute_changed,
                construction_stack: Vec::new(),
            })
        })();
        page.custom_elements.defining.set(false);
        let definition = read?;

        page.custom_elements
            .definitions
            .borrow_mut()
            .push(definition);
        // Elements already in the document are upgraded, in tree order.
        let candidates: Vec<NodeId> = {
            let dom = page.dom.borrow();
            dom.descendants(dom.document())
                .filter(|&n| {
                    dom.element(n).is_some_and(|e| {
                        e.is_html()
                            && *e.name.local == *local_name
                            && e.custom_element_state == CustomElementState::Undefined
                            && (local_name == name
                                || e.is_value.as_deref() == Some(&name)
                                || e.attr("is") == Some(&name))
                    })
                })
                .collect()
        };
        for element in candidates {
            let dom = page.dom.borrow();
            if let Some(index) = definition_for(page, &dom, element) {
                drop(dom);
                enqueue(page, element, Reaction::Upgrade(index));
            }
        }
        let waiting = page.custom_elements.when_defined.borrow_mut().remove(&name);
        for promise in waiting.unwrap_or_default() {
            cx.script
                .resolve_promise(&promise, Value::Callback(constructor.clone()));
        }
        Ok(())
    }

    fn get(cx: &mut Cx<'_>, _this: ObjectId, name: String) -> Fallible<Option<Callback>> {
        let definitions = cx.page.custom_elements.definitions.borrow();
        Ok(definitions
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.constructor.clone()))
    }

    fn when_defined(cx: &mut Cx<'_>, _this: ObjectId, name: String) -> Fallible<PromiseRef> {
        let promise = cx.script.new_promise();
        if !is_valid_custom_element_name(&name) {
            cx.script.reject_promise(
                &promise,
                Exception::syntax(format!("'{name}' is not a valid custom element name")),
            );
            return Ok(promise);
        }
        let defined = cx
            .page
            .custom_elements
            .definitions
            .borrow()
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.constructor.clone());
        match defined {
            Some(constructor) => cx
                .script
                .resolve_promise(&promise, Value::Callback(constructor)),
            None => cx
                .page
                .custom_elements
                .when_defined
                .borrow_mut()
                .entry(name)
                .or_default()
                .push(promise.clone()),
        }
        Ok(promise)
    }

    fn upgrade(cx: &mut Cx<'_>, _this: ObjectId, root: NodeId) -> Fallible<()> {
        node::check(cx, root)?;
        subtree_created(cx.page, root);
        Ok(())
    }
}
