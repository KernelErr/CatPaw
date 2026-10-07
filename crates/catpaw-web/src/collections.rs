//! `NodeList` and `HTMLCollection`.
//!
//! Live collections are recomputed lazily: each remembers the arena version
//! it was computed for and refreshes itself when the tree has changed since.

use catpaw_dom::{Dom, NodeId, QuirksMode};
use catpaw_js::{Fallible, ObjectId};

use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

#[derive(Clone, Debug)]
pub enum ListSource {
    /// `node.childNodes`.
    ChildNodes(NodeId),
    /// `parent.children`.
    ChildElements(NodeId),
    /// `getElementsByTagName`: the qualified name as given.
    TagName { root: NodeId, name: String },
    /// `getElementsByTagNameNS`: `*` matches any namespace or local name.
    TagNameNS {
        root: NodeId,
        namespace: String,
        local: String,
    },
    /// `getElementsByClassName`: the class tokens.
    ClassNames { root: NodeId, classes: Vec<String> },
    /// `document.getElementsByName`.
    Name { root: NodeId, name: String },
    /// `document.images` and the other collections of a document.
    DocumentKind { root: NodeId, kind: DocumentKind },
    /// `document[name]` when several elements share the name.
    DocumentNamed { root: NodeId, name: String },
    /// `form.elements`.
    FormControls(NodeId),
    /// `select.options`.
    Options(NodeId),
    /// A snapshot (`querySelectorAll`).
    Static,
}

/// The element collections a document exposes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocumentKind {
    /// `img` elements.
    Images,
    /// `embed` elements (`embeds` and `plugins`).
    Embeds,
    /// `a` and `area` elements with an `href`.
    Links,
    Forms,
    Scripts,
    /// `a` elements with a `name`.
    Anchors,
    /// Always empty: `applet` is no element any more.
    Applets,
}

impl DocumentKind {
    fn matches(self, el: &catpaw_dom::ElementData) -> bool {
        let local = &*el.name.local;
        el.is_html()
            && match self {
                DocumentKind::Images => local == "img",
                DocumentKind::Embeds => local == "embed",
                DocumentKind::Links => matches!(local, "a" | "area") && el.attr("href").is_some(),
                DocumentKind::Forms => local == "form",
                DocumentKind::Scripts => local == "script",
                DocumentKind::Anchors => local == "a" && el.attr("name").is_some(),
                DocumentKind::Applets => false,
            }
    }
}

fn qualified_name(el: &catpaw_dom::ElementData) -> String {
    match &el.name.prefix {
        Some(prefix) => format!("{}:{}", prefix, el.name.local),
        None => el.name.local.to_string(),
    }
}

fn compute(dom: &Dom, source: &ListSource) -> Vec<NodeId> {
    match source {
        ListSource::Static => Vec::new(),
        ListSource::ChildNodes(node) => {
            if dom.contains(*node) {
                dom.children(*node).collect()
            } else {
                Vec::new()
            }
        }
        ListSource::ChildElements(node) => {
            if dom.contains(*node) {
                dom.child_elements(*node).collect()
            } else {
                Vec::new()
            }
        }
        ListSource::TagName { root, name } => {
            if !dom.contains(*root) {
                return Vec::new();
            }
            let lower = name.to_ascii_lowercase();
            dom.descendants(*root)
                .filter(|&n| {
                    dom.element(n).is_some_and(|el| {
                        if name == "*" {
                            true
                        } else if el.is_html() {
                            qualified_name(el) == lower
                        } else {
                            qualified_name(el) == *name
                        }
                    })
                })
                .collect()
        }
        ListSource::TagNameNS {
            root,
            namespace,
            local,
        } => {
            if !dom.contains(*root) {
                return Vec::new();
            }
            dom.descendants(*root)
                .filter(|&n| {
                    dom.element(n).is_some_and(|el| {
                        (namespace == "*" || *el.name.ns == **namespace)
                            && (local == "*" || *el.name.local == **local)
                    })
                })
                .collect()
        }
        ListSource::ClassNames { root, classes } => {
            if classes.is_empty() || !dom.contains(*root) {
                return Vec::new();
            }
            let quirks = dom.quirks_mode() == QuirksMode::Quirks;
            dom.descendants(*root)
                .filter(|&n| {
                    dom.element(n).is_some_and(|el| {
                        classes.iter().all(|wanted| {
                            el.classes().any(|have| {
                                if quirks {
                                    have.eq_ignore_ascii_case(wanted)
                                } else {
                                    have == wanted
                                }
                            })
                        })
                    })
                })
                .collect()
        }
        ListSource::Name { root, name } => {
            if !dom.contains(*root) {
                return Vec::new();
            }
            dom.descendants(*root)
                .filter(|&n| {
                    dom.element(n)
                        .is_some_and(|el| el.is_html() && el.attr("name") == Some(name.as_str()))
                })
                .collect()
        }
        ListSource::DocumentKind { root, kind } => {
            if !dom.contains(*root) {
                return Vec::new();
            }
            dom.descendants(*root)
                .filter(|&n| dom.element(n).is_some_and(|el| kind.matches(el)))
                .collect()
        }
        ListSource::DocumentNamed { root, name } => {
            crate::document::named_elements(dom, *root, name)
        }
        ListSource::FormControls(form) => {
            if !dom.contains(*form) {
                return Vec::new();
            }
            crate::forms::listed_controls(dom, *form)
        }
        ListSource::Options(select) => {
            if !dom.contains(*select) {
                return Vec::new();
            }
            crate::forms::options_of(dom, *select)
        }
    }
}

struct ListState {
    source: ListSource,
    items: Vec<NodeId>,
    /// The arena version `items` was computed for.
    version: u64,
}

impl ListState {
    fn new(source: ListSource, items: Vec<NodeId>) -> Self {
        Self {
            source,
            items,
            // Forces a first computation for live lists.
            version: u64::MAX,
        }
    }
}

pub struct NodeListObject(ListState, InterfaceId);
platform_object!(NodeListObject, |l| l.1);

pub struct HtmlCollectionObject(ListState, InterfaceId);
platform_object!(HtmlCollectionObject, |c| c.1);

trait HasList: 'static {
    fn list(&mut self) -> &mut ListState;
}

impl HasList for NodeListObject {
    fn list(&mut self) -> &mut ListState {
        &mut self.0
    }
}

impl HasList for HtmlCollectionObject {
    fn list(&mut self) -> &mut ListState {
        &mut self.0
    }
}

/// A live `NodeList`.
pub fn node_list(page: &PageState, source: ListSource) -> ObjectId {
    page.alloc(NodeListObject(
        ListState::new(source, Vec::new()),
        InterfaceId::NodeList,
    ))
}

/// A static `NodeList` holding `items`.
pub fn static_node_list(page: &PageState, items: Vec<NodeId>) -> ObjectId {
    page.alloc(NodeListObject(
        ListState::new(ListSource::Static, items),
        InterfaceId::NodeList,
    ))
}

/// A static `RadioNodeList` holding `items`: the controls a form's
/// collection names, when there are several.
pub fn radio_node_list(page: &PageState, items: Vec<NodeId>) -> ObjectId {
    page.alloc(NodeListObject(
        ListState::new(ListSource::Static, items),
        InterfaceId::RadioNodeList,
    ))
}

/// A static `HTMLCollection` holding `items`.
pub fn static_html_collection(page: &PageState, items: Vec<NodeId>) -> ObjectId {
    page.alloc(HtmlCollectionObject(
        ListState::new(ListSource::Static, items),
        InterfaceId::HTMLCollection,
    ))
}

/// A live `HTMLCollection`.
pub fn html_collection(page: &PageState, source: ListSource) -> ObjectId {
    page.alloc(HtmlCollectionObject(
        ListState::new(source, Vec::new()),
        InterfaceId::HTMLCollection,
    ))
}

/// A live collection of a derived interface (`HTMLFormControlsCollection`,
/// `HTMLOptionsCollection`).
pub fn html_collection_as(page: &PageState, source: ListSource, iface: InterfaceId) -> ObjectId {
    page.alloc(HtmlCollectionObject(
        ListState::new(source, Vec::new()),
        iface,
    ))
}

/// The items of a node list or collection, whichever `id` is.
pub(crate) fn items_of(cx: &Cx<'_>, id: ObjectId) -> Fallible<Vec<NodeId>> {
    match with_items::<HtmlCollectionObject, _>(cx, id, |items| items.to_vec()) {
        Ok(items) => Ok(items),
        Err(_) => with_items::<NodeListObject, _>(cx, id, |items| items.to_vec()),
    }
}

/// Runs `f` on the current items of the list `id`.
fn with_items<T: HasList, R>(
    cx: &Cx<'_>,
    id: ObjectId,
    f: impl FnOnce(&[NodeId]) -> R,
) -> Fallible<R> {
    let version = cx.dom().version();
    let stale = cx.page.with::<T, _>(id, |o| {
        let list = o.list();
        let is_static = matches!(list.source, ListSource::Static);
        (!is_static && list.version != version).then(|| list.source.clone())
    })?;
    if let Some(source) = stale {
        let items = compute(&cx.dom(), &source);
        cx.page.with::<T, _>(id, |o| {
            let list = o.list();
            list.items = items;
            list.version = version;
        })?;
    }
    cx.page.with::<T, _>(id, |o| f(&o.list().items))
}

impl web::NodeListImpl for Web {
    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<NodeId>> {
        with_items::<NodeListObject, _>(cx, this, |items| items.get(index as usize).copied())
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        with_items::<NodeListObject, _>(cx, this, |items| items.len() as u32)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<NodeId>> {
        <Web as web::NodeListImpl>::item(cx, this, index)
    }
}

fn named_item(cx: &Cx<'_>, this: ObjectId, name: &str) -> Fallible<Option<NodeId>> {
    if name.is_empty() {
        return Ok(None);
    }
    let dom = cx.dom();
    with_items::<HtmlCollectionObject, _>(cx, this, |items| {
        items.iter().copied().find(|&n| {
            dom.element(n).is_some_and(|el| {
                el.id() == Some(name) || (el.is_html() && el.attr("name") == Some(name))
            })
        })
    })
}

impl web::HTMLCollectionImpl for Web {
    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        with_items::<HtmlCollectionObject, _>(cx, this, |items| items.len() as u32)
    }

    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<NodeId>> {
        with_items::<HtmlCollectionObject, _>(cx, this, |items| items.get(index as usize).copied())
    }

    fn named_item(cx: &mut Cx<'_>, this: ObjectId, name: String) -> Fallible<Option<NodeId>> {
        named_item(cx, this, &name)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<NodeId>> {
        <Web as web::HTMLCollectionImpl>::item(cx, this, index)
    }

    fn named_get(cx: &mut Cx<'_>, this: ObjectId, name: &str) -> Fallible<Option<NodeId>> {
        named_item(cx, this, name)
    }

    fn named_properties(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<String>> {
        let dom = cx.dom();
        with_items::<HtmlCollectionObject, _>(cx, this, |items| {
            let mut names: Vec<String> = Vec::new();
            let mut push = |name: &str| {
                if !name.is_empty() && !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            };
            for &n in items {
                let Some(el) = dom.element(n) else { continue };
                if let Some(id) = el.id() {
                    push(id);
                }
                if el.is_html()
                    && let Some(name) = el.attr("name")
                {
                    push(name);
                }
            }
            names
        })
    }
}
