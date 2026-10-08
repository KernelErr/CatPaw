//! The style engine: device, stylist, stylesheets, and the restyle driver.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, Once};

use catpaw_dom::{Dom, NodeId};
use selectors::matching::QuirksMode;
use style::context::{
    RegisteredSpeculativePainter, RegisteredSpeculativePainters, SharedStyleContext, StyleContext,
    ThreadLocalStyleContext,
};
use style::device::Device;
use style::dom::TNode;
use style::global_style_data::GLOBAL_STYLE_DATA;
use style::media_queries::{MediaList, MediaType};
use style::properties::style_structs::Font;
use style::properties::{ComputedValues, StyleBuilder};
use style::queries::values::PrefersColorScheme;
use style::selector_parser::{PseudoElement, SnapshotMap};
use style::servo::media_features::PointerCapabilities;
use style::servo_arc::Arc;
use style::shared_lock::{SharedRwLock, StylesheetGuards};
use style::stylesheets::{AllowImportRules, DocumentStyleSheet, Origin, Stylesheet, UrlExtraData};
use style::stylist::{RuleInclusion, Stylist};
use style::thread_state::{self, ThreadState};
use style::traversal::{DomTraversal, UndisplayedStyleCache, recalc_style_at, resolve_style};
use style::traversal_flags::TraversalFlags;
use style_dom::ElementState;
use url::Url;

use crate::computed::{ComputedStyle, Pseudo};
use crate::node::{CatNode, with_style_context};
use crate::table::StyleTable;

/// Engine construction options.
#[derive(Debug, Clone)]
pub struct StyleOptions {
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub device_pixel_ratio: f32,
    pub base_url: Url,
    pub dark_mode: bool,
}

impl Default for StyleOptions {
    fn default() -> Self {
        Self {
            viewport_width: 1280.0,
            viewport_height: 720.0,
            device_pixel_ratio: 1.0,
            base_url: Url::parse("about:blank").expect("valid URL"),
            dark_mode: false,
        }
    }
}

struct NoPainters;

impl RegisteredSpeculativePainters for NoPainters {
    fn get(&self, _name: &style::Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

pub(crate) fn set_prefs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        style_config::set_pref!("layout.grid.enabled", true);
        style_config::set_pref!("layout.flexbox.balance", true);
        style_config::set_pref!("layout.unimplemented", true);
        style_config::set_pref!("layout.columns.enabled", true);
        style_config::set_pref!("layout.css.basic-shape-shape.enabled", true);
        style_config::set_pref!("layout.css.tree-counting-functions.enabled", true);
        style_config::set_pref!("layout.css.progress-function.enabled", true);
        style_config::set_pref!("layout.variable_fonts.enabled", true);
        style_config::set_pref!("layout.threads", -1);
    });
}

pub(crate) fn make_device(options: &StyleOptions) -> Device {
    let viewport_size = euclid::Size2D::new(options.viewport_width, options.viewport_height);
    let device_size = euclid::Size2D::new(options.viewport_width, options.viewport_height)
        * options.device_pixel_ratio;
    let device_pixel_ratio = euclid::Scale::new(options.device_pixel_ratio);
    Device::new(
        MediaType::screen(),
        QuirksMode::NoQuirks,
        viewport_size,
        device_size,
        device_pixel_ratio,
        Box::new(crate::fonts::CatFontMetricsProvider::new()),
        ComputedValues::initial_values_with_font_override(Font::initial_values()),
        if options.dark_mode {
            PrefersColorScheme::Dark
        } else {
            PrefersColorScheme::Light
        },
        PointerCapabilities::default(),
        PointerCapabilities::default(),
    )
}

/// What changed in the computed styles since layout last asked
/// ([`StyleEngine::take_restyled`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Restyled {
    /// Every style may have changed: the document was styled from scratch.
    pub full: bool,
    /// Otherwise, the elements whose style (or that of their `::before` or
    /// `::after`) changed. Elements styled for the first time are not
    /// listed: they came with a change to the tree.
    pub elements: Vec<NodeId>,
}

/// How many restyled elements are kept for layout before the engine just
/// says that everything may have changed.
const RESTYLED_LIMIT: usize = 4096;

/// How many anonymous box styles are kept before the engine starts over.
const ANONYMOUS_LIMIT: usize = 1024;

/// A parent style, and the style of an anonymous block box in it.
type AnonymousStyle = (Arc<ComputedValues>, Arc<ComputedValues>);

/// Computes styles for one document.
///
/// The engine follows the document through the arena's journal of
/// changes: a restyle only restyles what the changes since the last one
/// can have affected (Stylo's invalidation, from snapshots of the
/// attributes elements had), and the document is styled from scratch only
/// at first, when the style sheets or quirks mode change, or when the
/// journal no longer reaches back to the last restyle.
pub struct StyleEngine {
    pub(crate) table: StyleTable,
    stylist: Stylist,
    pub(crate) snapshots: SnapshotMap,
    animations: style::animation::DocumentAnimationSet,
    pub(crate) url_data: UrlExtraData,
    pub(crate) quirks_mode: QuirksMode,
    /// The author stylesheets in cascade order, each with the key it was
    /// set under.
    author_sheets: Vec<(u64, DocumentStyleSheet)>,
    /// Styles resolved on demand, until the document next changes.
    pub(crate) resolved: UndisplayedStyleCache,
    /// The arena version the slots reflect, once they were made.
    pub(crate) synced: Option<u64>,
    /// The elements' style data is what a traversal computed, with the
    /// changes since recorded as restyle hints, snapshots and dirty bits.
    /// Otherwise there is no style data, and the next restyle styles the
    /// whole document.
    pub(crate) styled: bool,
    /// Restyle hints or snapshots wait for a traversal.
    pub(crate) pending: bool,
    /// The elements whose snapshots wait for the traversal.
    pub(crate) snapshotted: Vec<NodeId>,
    /// What changed for layout.
    restyled: Restyled,
    /// How many times the whole document was styled, and how many
    /// incremental restyles were done (for tests and profiling).
    counts: (u64, u64),
    /// The styles of anonymous block boxes, by the address of their
    /// parent's style (which each entry keeps alive).
    anonymous: RefCell<HashMap<usize, AnonymousStyle>>,
}

impl StyleEngine {
    pub fn new(options: &StyleOptions) -> Self {
        set_prefs();
        let lock = SharedRwLock::new();
        let mut stylist = Stylist::new(make_device(options), QuirksMode::NoQuirks);
        let url_data = UrlExtraData(Arc::new(options.base_url.clone()));
        let ua = make_stylesheet(
            crate::UA_STYLESHEET,
            Origin::UserAgent,
            &lock,
            &url_data,
            QuirksMode::NoQuirks,
        );
        stylist.append_stylesheet(ua, &lock.read());
        Self {
            table: StyleTable::new(lock),
            stylist,
            snapshots: SnapshotMap::new(),
            animations: Default::default(),
            url_data,
            quirks_mode: QuirksMode::NoQuirks,
            author_sheets: Vec::new(),
            resolved: UndisplayedStyleCache::default(),
            synced: None,
            styled: false,
            pending: false,
            snapshotted: Vec::new(),
            restyled: Restyled::default(),
            counts: (0, 0),
            anonymous: RefCell::new(HashMap::new()),
        }
    }

    pub fn table(&self) -> &StyleTable {
        &self.table
    }

    /// Sets the document's quirks mode (from the parser) before styling.
    /// A change restyles the whole document.
    pub fn set_quirks_mode(&mut self, mode: catpaw_dom::QuirksMode) {
        let mode = match mode {
            catpaw_dom::QuirksMode::Quirks => QuirksMode::Quirks,
            catpaw_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
            catpaw_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
        };
        if mode != self.quirks_mode {
            self.quirks_mode = mode;
            self.stylist.set_quirks_mode(mode);
            self.invalidate();
        }
    }

    /// Appends an author stylesheet (a `<style>` element's text or a fetched
    /// `<link rel=stylesheet>`), in document order.
    pub fn add_author_stylesheet(&mut self, css: &str) {
        let sheet = make_stylesheet(
            css,
            Origin::Author,
            self.table.lock(),
            &self.url_data,
            self.quirks_mode,
        );
        self.stylist
            .append_stylesheet(sheet.clone(), &self.table.lock().read());
        let key = self.author_sheets.len() as u64;
        self.author_sheets.push((key, sheet));
        self.invalidate();
    }

    /// Replaces the author stylesheets with `sheets`, given in cascade
    /// order. Each comes with a key standing for its text: a sheet whose
    /// key is already in use is kept as it is rather than parsed again.
    /// Keys must be unique. Returns whether anything changed.
    ///
    /// When the sheets that stay keep their order, the others are removed
    /// and inserted around them, and the next restyle restyles what Stylo
    /// finds their rules can match (often everything, for a sheet with
    /// rules it cannot narrow down to an id, class or element name).
    /// Otherwise the whole document is restyled from scratch.
    pub fn set_author_stylesheets(&mut self, sheets: &[(u64, &str)]) -> bool {
        let unchanged = sheets.len() == self.author_sheets.len()
            && sheets
                .iter()
                .zip(&self.author_sheets)
                .all(|((key, _), (old, _))| key == old);
        if unchanged {
            return false;
        }
        let lock = self.table.lock().clone();
        let guard = lock.read();
        let stays = |key: &u64| sheets.iter().any(|(k, _)| k == key);
        // The sheets that stay, in their old order and in their new one.
        let kept_before: Vec<u64> = self
            .author_sheets
            .iter()
            .map(|(key, _)| *key)
            .filter(stays)
            .collect();
        let kept_after: Vec<u64> = sheets
            .iter()
            .map(|(key, _)| *key)
            .filter(|key| self.author_sheets.iter().any(|(k, _)| k == key))
            .collect();
        let in_place = kept_before == kept_after;
        let mut old: HashMap<u64, DocumentStyleSheet> = self.author_sheets.drain(..).collect();
        if !in_place {
            for sheet in old.values() {
                self.stylist.remove_stylesheet(sheet.clone(), &guard);
            }
            for &(key, css) in sheets {
                let sheet = old.remove(&key).unwrap_or_else(|| {
                    make_stylesheet(css, Origin::Author, &lock, &self.url_data, self.quirks_mode)
                });
                self.stylist.append_stylesheet(sheet.clone(), &guard);
                self.author_sheets.push((key, sheet));
            }
            drop(guard);
            self.invalidate();
            return true;
        }
        for (key, sheet) in &old {
            if !stays(key) {
                self.stylist.remove_stylesheet(sheet.clone(), &guard);
            }
        }
        for (i, &(key, css)) in sheets.iter().enumerate() {
            let sheet = match old.get(&key) {
                Some(sheet) => sheet.clone(),
                None => {
                    let sheet = make_stylesheet(
                        css,
                        Origin::Author,
                        &lock,
                        &self.url_data,
                        self.quirks_mode,
                    );
                    // Before the next sheet that stays, if there is one.
                    let before = sheets[i + 1..].iter().find_map(|(k, _)| old.get(k));
                    match before {
                        Some(before) => self.stylist.insert_stylesheet_before(
                            sheet.clone(),
                            before.clone(),
                            &guard,
                        ),
                        None => self.stylist.append_stylesheet(sheet.clone(), &guard),
                    }
                    sheet
                }
            };
            self.author_sheets.push((key, sheet));
        }
        drop(guard);
        self.resolved.clear();
        if self.styled {
            // The traversal flushes the stylist, which says what to restyle.
            self.pending = true;
        }
        true
    }

    /// Tells the engine that every style may have changed (the sheets or
    /// what they are matched against changed): the style data is dropped
    /// and the next restyle styles the whole document. Changes to the
    /// document itself need no telling: the engine reads them from the
    /// arena's journal.
    pub fn invalidate(&mut self) {
        self.anonymous.get_mut().clear();
        self.resolved.clear();
        self.table.clear_data();
        self.snapshots.clear();
        self.snapshotted.clear();
        self.styled = false;
        self.pending = false;
    }

    /// What changed in the computed styles since the last call: whether
    /// the document was styled from scratch, or which elements changed.
    pub fn take_restyled(&mut self) -> Restyled {
        std::mem::take(&mut self.restyled)
    }

    /// How many times the whole document was styled, and how many
    /// incremental restyles found something to do.
    pub fn restyle_counts(&self) -> (u64, u64) {
        self.counts
    }

    /// Whether the style data reflects the document as it is: a restyle
    /// now would have nothing to do.
    pub fn is_fresh(&self, dom: &Dom) -> bool {
        self.styled && !self.pending && self.synced == Some(dom.version())
    }

    pub fn author_sheet_count(&self) -> usize {
        self.author_sheets.len()
    }

    /// The computed style of a connected element or one of its
    /// pseudo-elements, resolved on demand: only the element and those of
    /// its ancestors not resolved since the last [`StyleEngine::invalidate`]
    /// are styled. `None` for nodes that are not elements in the document.
    pub fn computed_style(
        &mut self,
        dom: &Dom,
        id: NodeId,
        pseudo: Option<Pseudo>,
    ) -> Option<ComputedStyle> {
        if !dom.contains(id) || !dom.is_element(id) || !dom.is_connected(id) {
            return None;
        }
        if self.styled {
            // Catching up with a few changes costs less than resolving
            // the element and its ancestors from scratch.
            self.restyle(dom);
            if let (None, Some(style)) = (pseudo, self.primary_style(id)) {
                return Some(ComputedStyle(style));
            }
        } else {
            self.sync(dom);
        }
        if let (None, Some(style)) = (pseudo, self.resolved.get(&CatNode::new(id).opaque_id())) {
            return Some(ComputedStyle(style.clone()));
        }
        let root = dom.child_elements(dom.document()).next()?;

        let Self {
            table,
            stylist,
            snapshots,
            animations,
            resolved,
            ..
        } = self;
        let style = with_style_context(dom, table, || {
            thread_state::enter(ThreadState::LAYOUT);
            let lock = table.lock().clone();
            let guard = lock.read();
            let guards = StylesheetGuards {
                author: &guard,
                ua_or_user: &guard,
            };
            stylist
                .flush(&guards)
                .process_style(CatNode::new(root), Some(&*snapshots));

            let shared = SharedStyleContext {
                stylist,
                visited_styles_enabled: false,
                options: GLOBAL_STYLE_DATA.options.clone(),
                guards,
                current_time_for_animations: 0.0,
                traversal_flags: TraversalFlags::empty(),
                snapshot_map: snapshots,
                animations: animations.clone(),
                registered_speculative_painters: &NoPainters,
            };
            // Dropped before the thread leaves the layout state.
            let mut thread_local = ThreadLocalStyleContext::new();
            let mut context = StyleContext {
                shared: &shared,
                thread_local: &mut thread_local,
            };
            let pseudo_element = pseudo.map(|pseudo| match pseudo {
                Pseudo::Before => PseudoElement::Before,
                Pseudo::After => PseudoElement::After,
            });
            let styles = resolve_style(
                &mut context,
                CatNode::new(id),
                RuleInclusion::All,
                pseudo_element.as_ref(),
                Some(resolved),
            );
            let style = match &pseudo_element {
                None => styles.primary().clone(),
                Some(pseudo_element) => match styles.pseudos.get(pseudo_element) {
                    Some(style) => style.clone(),
                    // No rule gives the pseudo-element content: its style
                    // is what it inherits.
                    None => StyleBuilder::for_inheritance(
                        stylist.device(),
                        Some(stylist),
                        Some(styles.primary()),
                        Some(pseudo_element),
                    )
                    .build(),
                },
            };
            drop(thread_local);
            thread_state::exit(ThreadState::LAYOUT);
            style
        });
        Some(ComputedStyle(style))
    }

    /// Brings the slots of the connected elements (and the attribute
    /// derived data Stylo reads during matching) up to date with the
    /// document, and records what the changes since the last time mean
    /// for the styles.
    pub fn ensure_slots(&mut self, dom: &Dom) {
        self.sync(dom);
    }

    /// Brings the styles of the whole document up to date: from scratch
    /// the first time and after [`StyleEngine::invalidate`], otherwise
    /// restyling only what changed since the last restyle.
    pub fn restyle(&mut self, dom: &Dom) {
        self.sync(dom);
        if self.styled && !self.pending {
            return;
        }
        let full = !self.styled;
        let Some(root) = dom.child_elements(dom.document()).next() else {
            self.styled = true;
            self.pending = false;
            self.clear_snapshots();
            return;
        };
        if full {
            self.table.clear_data();
            self.clear_snapshots();
        }
        let restyled = self.traverse(dom, root);
        self.clear_snapshots();
        self.styled = true;
        self.pending = false;
        if full {
            self.counts.0 += 1;
            self.restyled = Restyled {
                full: true,
                elements: Vec::new(),
            };
        } else {
            self.counts.1 += 1;
            if !self.restyled.full {
                self.restyled.elements.extend(restyled);
                if self.restyled.elements.len() > RESTYLED_LIMIT {
                    self.restyled = Restyled {
                        full: true,
                        elements: Vec::new(),
                    };
                }
            }
        }
    }

    /// Drops the snapshots taken for a traversal, which has seen them.
    fn clear_snapshots(&mut self) {
        self.snapshots.clear();
        for id in self.snapshotted.drain(..) {
            if let Some(slot) = self.table.slot(id) {
                slot.has_snapshot.store(false, Ordering::SeqCst);
                slot.snapshot_handled.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Runs Stylo's traversal from the root element: every element without
    /// style data is styled, the others as their restyle hints, snapshots
    /// and dirty bits say. Returns the elements whose style changed.
    fn traverse(&mut self, dom: &Dom, root: NodeId) -> Vec<NodeId> {
        let Self {
            table,
            stylist,
            snapshots,
            animations,
            ..
        } = self;
        with_style_context(dom, table, || {
            thread_state::enter(ThreadState::LAYOUT);
            let lock = table.lock().clone();
            let guard = lock.read();
            let guards = StylesheetGuards {
                author: &guard,
                ua_or_user: &guard,
            };

            let root_node = CatNode::new(root);
            stylist
                .flush(&guards)
                .process_style(root_node, Some(&*snapshots));

            let context = SharedStyleContext {
                stylist,
                visited_styles_enabled: false,
                options: GLOBAL_STYLE_DATA.options.clone(),
                guards,
                current_time_for_animations: 0.0,
                traversal_flags: TraversalFlags::empty(),
                snapshot_map: snapshots,
                animations: animations.clone(),
                registered_speculative_painters: &NoPainters,
            };
            let token = RecalcStyle::pre_traverse(root_node, &context);
            let mut restyled = Vec::new();
            if token.should_traverse() {
                let traversal = RecalcStyle {
                    context,
                    restyled: Mutex::new(Vec::new()),
                };
                style::driver::traverse_dom(&traversal, token, None);
                restyled = traversal
                    .restyled
                    .into_inner()
                    .unwrap_or_else(|e| e.into_inner());
            }
            stylist.rule_tree().maybe_gc();
            thread_state::exit(ThreadState::LAYOUT);
            restyled
        })
    }

    /// The style of the element's `::before` or `::after`, if the last
    /// restyle gave it one (it has `content`).
    pub fn pseudo_style(&self, id: NodeId, pseudo: Pseudo) -> Option<Arc<ComputedValues>> {
        let slot = self.table.slot(id)?;
        if !slot.has_data.load(Ordering::SeqCst) {
            return None;
        }
        let data = slot.data.borrow();
        let pseudo = match pseudo {
            Pseudo::Before => PseudoElement::Before,
            Pseudo::After => PseudoElement::After,
        };
        data.styles.pseudos.get(&pseudo).cloned()
    }

    /// The style of an anonymous block box generated inside an element with
    /// `parent` style: inherited properties come from the parent, the rest
    /// are initial, and `display` is `block`. The same parent style gets
    /// the same `Arc` back, so that layout can tell the box unchanged.
    pub fn anonymous_block_style(&self, parent: &Arc<ComputedValues>) -> Arc<ComputedValues> {
        let key = parent.heap_ptr() as usize;
        if let Some((_, style)) = self.anonymous.borrow().get(&key) {
            return style.clone();
        }
        let lock = self.table.lock().clone();
        let guard = lock.read();
        let guards = StylesheetGuards {
            author: &guard,
            ua_or_user: &guard,
        };
        let style = self.stylist.style_for_anonymous::<CatNode>(
            &guards,
            &PseudoElement::ServoAnonymousBox,
            parent,
        );
        let mut anonymous = self.anonymous.borrow_mut();
        if anonymous.len() >= ANONYMOUS_LIMIT {
            anonymous.clear();
        }
        // The parent is kept alive with its entry, so that its address
        // stands for it.
        anonymous.insert(key, (parent.clone(), style.clone()));
        style
    }

    /// The element's primary computed style, if it was styled.
    pub fn primary_style(&self, id: NodeId) -> Option<Arc<ComputedValues>> {
        let slot = self.table.slot(id)?;
        if !slot.has_data.load(Ordering::SeqCst) {
            return None;
        }
        let data = slot.data.borrow();
        data.styles.get_primary().cloned()
    }

    /// `display: none` on the element, or membership in a `display: none`
    /// subtree (Stylo does not style those, so a connected element without
    /// style data is hidden).
    pub fn is_display_none(&self, dom: &Dom, id: NodeId) -> bool {
        if !dom.is_element(id) {
            return false;
        }
        match self.primary_style(id) {
            Some(style) => style.get_box().display.is_none(),
            None => dom.is_connected(id),
        }
    }

    /// `visibility: hidden | collapse` on the element itself.
    pub fn is_visibility_hidden(&self, id: NodeId) -> bool {
        use style::properties::longhands::visibility::computed_value::T as Visibility;
        self.primary_style(id)
            .is_some_and(|style| style.get_inherited_box().visibility != Visibility::Visible)
    }

    /// Whether the element's box is block-level (block, flex, grid, list
    /// items, table parts: anything not laid out within a line); `None`
    /// without style data.
    pub fn is_block_level(&self, id: NodeId) -> Option<bool> {
        use style::values::specified::box_::DisplayOutside;
        let display = self.primary_style(id)?.get_box().display;
        Some(matches!(
            display.outside(),
            DisplayOutside::Block | DisplayOutside::TableCaption | DisplayOutside::InternalTable
        ))
    }

    /// `cursor: pointer` on the element (the property inherits, so this is
    /// also true inside a pointer-cursor ancestor).
    pub fn is_pointer_cursor(&self, id: NodeId) -> bool {
        use style::values::computed::ui::CursorKind;
        self.primary_style(id)
            .is_some_and(|style| style.get_inherited_ui().cursor.keyword == CursorKind::Pointer)
    }
}

fn make_stylesheet(
    css: &str,
    origin: Origin,
    lock: &SharedRwLock,
    url_data: &UrlExtraData,
    quirks_mode: QuirksMode,
) -> DocumentStyleSheet {
    let sheet = Stylesheet::from_str(
        css,
        url_data.clone(),
        origin,
        Arc::new(lock.wrap(MediaList::empty())),
        lock.clone(),
        None,
        None,
        quirks_mode,
        AllowImportRules::Yes,
    );
    DocumentStyleSheet(Arc::new(sheet))
}

/// Pseudo-class state from markup alone. The engine will extend this with
/// live state (hover, focus, checkedness changed by script or the user).
pub(crate) fn element_state(dom: &Dom, id: NodeId) -> ElementState {
    let mut state = ElementState::empty();
    let Some(el) = dom.element(id) else {
        return state;
    };
    if !el.is_html() {
        return state;
    }
    let local = &*el.name.local;
    if matches!(local, "a" | "area" | "link") && el.has_attr("href") {
        state.insert(ElementState::UNVISITED);
    }
    if matches!(
        local,
        "button" | "input" | "select" | "textarea" | "optgroup" | "option" | "fieldset"
    ) {
        let disabled = el.has_attr("disabled")
            || dom
                .ancestors(id)
                .any(|a| dom.is_html_element(a, "fieldset") && dom.attr(a, "disabled").is_some());
        state.insert(if disabled {
            ElementState::DISABLED
        } else {
            ElementState::ENABLED
        });
    }
    if local == "input" {
        let ty = el
            .attr("type")
            .map(|t| t.to_ascii_lowercase())
            .unwrap_or_else(|| "text".to_string());
        if matches!(ty.as_str(), "checkbox" | "radio") && el.has_attr("checked") {
            state.insert(ElementState::CHECKED);
        }
    }
    if local == "option" && el.has_attr("selected") {
        state.insert(ElementState::CHECKED);
    }
    if matches!(local, "input" | "select" | "textarea") {
        state.insert(if el.has_attr("required") {
            ElementState::REQUIRED
        } else {
            ElementState::OPTIONAL_
        });
    }
    state
}

/// The traversal: style each element in pre-order.
struct RecalcStyle<'a> {
    context: SharedStyleContext<'a>,
    /// The elements whose style changed (they have restyle damage).
    restyled: Mutex<Vec<NodeId>>,
}

#[allow(unsafe_code)]
impl DomTraversal<CatNode> for RecalcStyle<'_> {
    fn process_preorder<F: FnMut(CatNode)>(
        &self,
        context: &mut StyleContext<CatNode>,
        node: CatNode,
        note_child: F,
    ) {
        if let Some(el) = node.as_element() {
            // SAFETY: the traversal has exclusive access to the style table.
            let mut data = unsafe { style::dom::TElement::ensure_data(&el) };
            recalc_style_at(self, context, el, &mut data, note_child);
            // There is no layout pass to consume the damage: it is noted
            // here and cleared, so that the next traversal does not visit
            // the element for it.
            if !data.damage.is_empty() {
                self.restyled
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(el.id);
            }
            data.clear_restyle_flags_and_damage();
            unsafe { style::dom::TElement::unset_dirty_descendants(&el) };
        }
    }

    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(&self, _context: &mut StyleContext<CatNode>, _node: CatNode) {
        unreachable!("needs_postorder_traversal is false")
    }

    fn shared_context(&self) -> &SharedStyleContext<'_> {
        &self.context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catpaw_dom::{NodeKind, parse_html};

    fn find(dom: &Dom, id: &str) -> NodeId {
        dom.descendants(dom.document())
            .find(|&n| dom.attr(n, "id") == Some(id))
            .unwrap()
    }

    #[test]
    fn resolves_display_from_ua_author_and_inline_styles() {
        let r = parse_html(
            r#"<!doctype html><style>.gone{display:none} #v{visibility:hidden}</style>
               <div id=a class=gone><p id=inner>x</p></div>
               <p id=b style="display: none"></p>
               <span id=c hidden></span>
               <script id=s></script>
               <div id=d>visible</div>
               <span id=v>hidden text</span>
               <template id=t><p id=tp></p></template>"#,
            &Default::default(),
        );
        let dom = &r.dom;
        let mut engine = StyleEngine::new(&StyleOptions::default());
        engine.set_quirks_mode(dom.quirks_mode());
        for n in dom.descendants(dom.document()) {
            if dom.is_html_element(n, "style") {
                engine.add_author_stylesheet(&dom.text_content(n));
            }
        }
        engine.restyle(dom);

        assert!(
            engine.is_display_none(dom, find(dom, "a")),
            "author display:none"
        );
        assert!(
            engine.is_display_none(dom, find(dom, "inner")),
            "inside a display:none subtree"
        );
        assert!(engine.is_display_none(dom, find(dom, "b")), "inline style");
        assert!(
            engine.is_display_none(dom, find(dom, "c")),
            "hidden attribute"
        );
        assert!(
            engine.is_display_none(dom, find(dom, "s")),
            "script via UA sheet"
        );
        assert!(!engine.is_display_none(dom, find(dom, "d")));
        assert!(engine.is_visibility_hidden(find(dom, "v")));
        assert!(!engine.is_visibility_hidden(find(dom, "d")));
        let style = engine.primary_style(find(dom, "d")).unwrap();
        assert!(!style.get_box().display.is_none());
    }

    #[test]
    fn resolves_styles_on_demand() {
        let r = parse_html(
            r#"<!doctype html><style>
                 :root { --gap: 4px; color: rgb(1, 2, 3); }
                 .box { margin: 1px 2px; display: flex; width: 50%; --own: red; }
                 .box > p:nth-child(2) { color: blue; opacity: 0.5; }
                 #b::before { content: "x"; color: green; }
                 .gone { display: none; }
                 .late { color: red; }
               </style>
               <div id=a class=box><p id=first>1</p><p id=second>2</p></div>
               <div class=gone><span id=hidden>h</span></div>
               <b id=b></b><i id=plain></i>"#,
            &Default::default(),
        );
        let mut dom = r.dom;
        let mut engine = StyleEngine::new(&StyleOptions::default());
        engine.set_quirks_mode(dom.quirks_mode());
        let sheets: Vec<String> = dom
            .descendants(dom.document())
            .filter(|&n| dom.is_html_element(n, "style"))
            .map(|n| dom.text_content(n))
            .collect();
        let keyed: Vec<(u64, &str)> = sheets.iter().map(|css| (7, css.as_str())).collect();
        engine.set_author_stylesheets(&keyed);

        fn get(
            engine: &mut StyleEngine,
            dom: &Dom,
            id: &str,
            pseudo: Option<Pseudo>,
            property: &str,
        ) -> String {
            engine
                .computed_style(dom, find(dom, id), pseudo)
                .expect("a connected element")
                .get(property)
        }

        for (id, property, expected) in [
            ("a", "display", "flex"),
            ("a", "margin", "1px 2px"),
            ("a", "margin-top", "1px"),
            ("a", "width", "50%"),
            ("a", "color", "rgb(1, 2, 3)"),
            ("a", "--gap", "4px"),
            ("a", "--own", "red"),
            ("a", "BACKGROUND-COLOR", "rgba(0, 0, 0, 0)"),
            ("a", "no-such-property", ""),
            ("first", "color", "rgb(1, 2, 3)"),
            ("second", "color", "rgb(0, 0, 255)"),
            ("second", "opacity", "0.5"),
            ("second", "--own", "red"),
            // Elements inside a `display: none` subtree have styles too.
            ("hidden", "display", "inline"),
        ] {
            assert_eq!(
                get(&mut engine, &dom, id, None, property),
                expected,
                "{id} {property}"
            );
        }
        let a = engine.computed_style(&dom, find(&dom, "a"), None).unwrap();
        assert_eq!(a.custom_properties(), ["--gap", "--own"]);

        for (id, pseudo, property, expected) in [
            ("b", Pseudo::Before, "content", "\"x\""),
            ("b", Pseudo::Before, "color", "rgb(0, 128, 0)"),
            ("b", Pseudo::After, "color", "rgb(1, 2, 3)"),
            ("plain", Pseudo::Before, "content", "none"),
            ("plain", Pseudo::Before, "display", "inline"),
        ] {
            assert_eq!(
                get(&mut engine, &dom, id, Some(pseudo), property),
                expected,
                "{id} {pseudo:?} {property}"
            );
        }

        // Nodes outside the document have no style.
        let text = dom.create_text("t");
        assert!(engine.computed_style(&dom, text, None).is_none());
        let detached = dom.create_html_element("p", Vec::new());
        assert!(engine.computed_style(&dom, detached, None).is_none());

        // After a change the engine is told, and resolves again.
        let first = find(&dom, "first");
        dom.element_mut(first)
            .unwrap()
            .attrs
            .push(catpaw_dom::Attr::html("class", "late"));
        engine.invalidate();
        assert_eq!(
            get(&mut engine, &dom, "first", None, "color"),
            "rgb(255, 0, 0)"
        );

        // A sheet set under the same key is kept; a new key replaces it.
        engine.set_author_stylesheets(&[(7, "ignored: the key is known")]);
        assert_eq!(get(&mut engine, &dom, "a", None, "display"), "flex");
        engine.set_author_stylesheets(&[(8, "#a { display: grid }")]);
        assert_eq!(get(&mut engine, &dom, "a", None, "display"), "grid");
        assert_eq!(get(&mut engine, &dom, "a", None, "margin-top"), "0px");

        let names = crate::computed::longhand_names();
        assert!(names.contains(&"display") && names.contains(&"margin-top"));
        assert!(!names.contains(&"margin"), "shorthands are not listed");
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    }

    const SHEET: &str = r#"
        .a { color: rgb(1, 0, 0) }
        .a .b { margin-top: 3px }
        .a > .c { display: none }
        #x { width: 10px }
        [data-k="1"] { font-size: 20px }
        [data-k="1"] + p { color: rgb(0, 2, 0) }
        .s ~ span { background-color: rgb(0, 0, 3) }
        li:first-child { padding-left: 1px }
        li:last-child { padding-right: 2px }
        li:nth-child(2n) { border-top-width: 4px; border-top-style: solid }
        li:nth-last-child(2) { margin-left: 7px }
        div:empty { height: 5px }
        div:empty + span { word-spacing: 3px }
        .h { display: none }
        .h + p { visibility: hidden }
        p::before { content: "x" }
        .q::after { content: "y"; color: rgb(9, 9, 9) }
        input:disabled { opacity: 0.5 }
        input:checked { opacity: 0.25 }
        fieldset[disabled] { color: rgb(4, 4, 4) }
        a:link { text-decoration-line: underline }
        .a span:not(.b) { letter-spacing: 1px }
        dt:first-child { padding-left: 1px }
        dt:last-child { padding-right: 2px }
    "#;

    const BODY: &str = r#"<!doctype html><body><div id=root>
        <ul id=list><li>1</li><li class=a>2</li><li>3</li></ul>
        <dl id=terms><dt>1</dt><dt>2</dt><dt>3</dt></dl>
        <div class=a><span class=b>b</span><span class=c>c</span><span>d</span></div>
        <div id=e></div><span>after e</span>
        <p data-k=0>p1</p><p>p2</p>
        <span class=s>s</span><span>t</span><span>u</span>
        <fieldset><input id=i type=checkbox><input></fieldset>
        <a href=x>link</a><a>not a link</a>
        </div>"#;

    /// A style engine over `dom` with [`SHEET`], styled from scratch.
    fn fresh_engine(dom: &Dom) -> StyleEngine {
        fresh_engine_with(dom, &[(1, SHEET)])
    }

    /// A style engine over `dom` with these sheets, styled from scratch.
    fn fresh_engine_with(dom: &Dom, sheets: &[(u64, &str)]) -> StyleEngine {
        let mut engine = StyleEngine::new(&StyleOptions::default());
        engine.set_quirks_mode(dom.quirks_mode());
        engine.set_author_stylesheets(sheets);
        engine.restyle(dom);
        engine
    }

    /// What the engine says about every connected element: the values of
    /// the properties [`SHEET`] sets, and its pseudo-elements.
    fn styles_of(engine: &StyleEngine, dom: &Dom) -> Vec<String> {
        const PROPERTIES: &[&str] = &[
            "display",
            "color",
            "margin-top",
            "margin-left",
            "width",
            "height",
            "font-size",
            "visibility",
            "background-color",
            "padding-left",
            "padding-right",
            "border-top-width",
            "opacity",
            "text-decoration-line",
            "letter-spacing",
            "word-spacing",
        ];
        let mut out = Vec::new();
        for n in dom.descendants(dom.document()) {
            let Some(el) = dom.element(n) else {
                continue;
            };
            let mut line = format!("{} {:?}:", el.name.local, el.attrs);
            match engine.primary_style(n) {
                Some(style) => {
                    let style = ComputedStyle(style);
                    for p in PROPERTIES {
                        line.push_str(&format!(" {p}={}", style.get(p)));
                    }
                }
                None => line.push_str(" unstyled"),
            }
            for pseudo in [Pseudo::Before, Pseudo::After] {
                if let Some(style) = engine.pseudo_style(n, pseudo) {
                    let style = ComputedStyle(style);
                    line.push_str(&format!(
                        " {pseudo:?}({} {})",
                        style.get("content"),
                        style.get("color")
                    ));
                }
            }
            out.push(line);
        }
        out
    }

    /// A small deterministic generator for the mutations.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n.max(1)
        }
    }

    fn attr_name(local: &str) -> catpaw_dom::QualName {
        catpaw_dom::QualName::new(None, catpaw_dom::ns!(), catpaw_dom::LocalName::from(local))
    }

    /// Sets an attribute to `value`, or removes it when it has that value.
    fn toggle(dom: &mut Dom, el: NodeId, local: &str, value: &str) {
        let data = dom.element_mut(el).unwrap();
        if data.attr(local) == Some(value) {
            data.remove_attr(local);
        } else {
            data.set_attr(attr_name(local), value);
        }
    }

    /// Changes the document at random, the way script would.
    fn mutate(dom: &mut Dom, rng: &mut Lcg, [root, list, terms]: [NodeId; 3]) -> String {
        let elements: Vec<NodeId> = dom
            .descendants(root)
            .filter(|&n| dom.is_element(n))
            .collect();
        let target = elements[rng.next(elements.len())];
        match rng.next(12) {
            0 => {
                let class = ["a", "b", "c", "h", "s", "q"][rng.next(6)];
                let data = dom.element_mut(target).unwrap();
                let mut classes: Vec<String> = data.classes().map(str::to_string).collect();
                if let Some(i) = classes.iter().position(|c| c == class) {
                    classes.remove(i);
                } else {
                    classes.push(class.to_string());
                }
                data.set_attr(attr_name("class"), classes.join(" "));
                format!("class {class}")
            }
            1 => {
                toggle(dom, target, "id", "x");
                "id x".into()
            }
            2 => {
                let value = ["0", "1"][rng.next(2)];
                dom.element_mut(target)
                    .unwrap()
                    .set_attr(attr_name("data-k"), value);
                format!("data-k {value}")
            }
            3 => {
                toggle(dom, target, "style", "color: rgb(5, 5, 5); display: block");
                "style".into()
            }
            4 | 5 => {
                // Lists half of the time, where the place of a child
                // matters to selectors.
                let (local, parent) = match rng.next(4) {
                    0 => ("li", list),
                    1 => ("dt", terms),
                    _ => (
                        ["li", "span", "div", "p", "dt"][rng.next(5)],
                        elements[rng.next(elements.len())],
                    ),
                };
                let new = dom.create_html_element(local, Vec::new());
                if rng.next(2) == 0 {
                    let text = dom.create_text("n");
                    dom.append_child(new, text);
                }
                let children: Vec<NodeId> = dom.children(parent).collect();
                let reference = rng.next(children.len() + 1);
                dom.insert_before(parent, new, children.get(reference).copied());
                format!("insert {local}")
            }
            6 => {
                let from = [list, terms][rng.next(2)];
                let children: Vec<NodeId> = dom.child_elements(from).collect();
                let gone = match (rng.next(2), children.len()) {
                    (0, n) if n > 0 => children[rng.next(n)],
                    _ => target,
                };
                if gone != root {
                    dom.detach(gone);
                }
                "remove".into()
            }
            7 => {
                match dom.first_child(target) {
                    Some(child) if dom.node(child).is_text() => match rng.next(2) {
                        0 => dom.remove_subtree(child),
                        _ => {
                            let empty = dom.node(child).as_text() == Some("");
                            let text = if empty { "t" } else { "" };
                            dom.node_mut(child).kind = NodeKind::Text(text.into());
                        }
                    },
                    _ => {
                        let text = dom.create_text(["", "t"][rng.next(2)]);
                        dom.append_child(target, text);
                    }
                }
                "text".into()
            }
            8 => {
                let fieldset = dom
                    .descendants(root)
                    .find(|&n| dom.is_html_element(n, "fieldset"));
                if let Some(fieldset) = fieldset {
                    toggle(dom, fieldset, "disabled", "");
                }
                let input = dom
                    .descendants(root)
                    .find(|&n| dom.attr(n, "id") == Some("i"));
                if let Some(input) = input {
                    toggle(dom, input, "checked", "");
                }
                "form state".into()
            }
            9 => {
                let link = dom.descendants(root).find(|&n| dom.is_html_element(n, "a"));
                if let Some(link) = link {
                    toggle(dom, link, "href", "y");
                }
                "href".into()
            }
            10 => {
                let to = elements[rng.next(elements.len())];
                if target != root && to != target && !dom.ancestors(to).any(|a| a == target) {
                    dom.append_child(to, target);
                }
                "move".into()
            }
            _ => {
                // A write that changes nothing selectors see.
                let value = dom.attr(target, "class").unwrap_or_default().to_string();
                dom.element_mut(target)
                    .unwrap()
                    .set_attr(attr_name("class"), value);
                "no-op".into()
            }
        }
    }

    #[test]
    fn incremental_restyles_match_restyling_from_scratch() {
        for seed in 1..=12u64 {
            let mut dom = parse_html(BODY, &Default::default()).dom;
            let mut engine = fresh_engine(&dom);
            let mut rng = Lcg(seed);
            let fixed = [find(&dom, "root"), find(&dom, "list"), find(&dom, "terms")];
            for step in 0..40 {
                let what = mutate(&mut dom, &mut rng, fixed);
                engine.restyle(&dom);
                let expected = styles_of(&fresh_engine(&dom), &dom);
                let actual = styles_of(&engine, &dom);
                assert_eq!(actual.len(), expected.len());
                for (a, e) in actual.iter().zip(&expected) {
                    assert_eq!(a, e, "seed {seed} step {step} ({what})");
                }
            }
            assert_eq!(engine.restyle_counts().0, 1, "styled from scratch once");
        }
    }

    #[test]
    fn children_coming_and_going_restyle_their_siblings() {
        let mut dom = parse_html(BODY, &Default::default()).dom;
        let mut engine = fresh_engine(&dom);
        let list = find(&dom, "list");
        let style = |engine: &StyleEngine, dom: &Dom, n: usize, property: &str| {
            let li = dom.child_elements(list).nth(n).unwrap();
            ComputedStyle(engine.primary_style(li).unwrap()).get(property)
        };
        assert_eq!(style(&engine, &dom, 0, "padding-left"), "1px");
        assert_eq!(style(&engine, &dom, 1, "border-top-width"), "4px");
        assert_eq!(style(&engine, &dom, 1, "margin-left"), "7px");
        assert_eq!(style(&engine, &dom, 2, "padding-right"), "2px");
        // A new first item: the old first is second now.
        let first = dom.first_child(list).unwrap();
        let li = dom.create_html_element("li", Vec::new());
        dom.insert_before(list, li, Some(first));
        engine.restyle(&dom);
        assert_eq!(style(&engine, &dom, 0, "padding-left"), "1px");
        assert_eq!(style(&engine, &dom, 1, "padding-left"), "0px");
        assert_eq!(style(&engine, &dom, 1, "border-top-width"), "4px");
        assert_eq!(style(&engine, &dom, 2, "border-top-width"), "0px");
        assert_eq!(style(&engine, &dom, 2, "margin-left"), "7px");
        // The last one goes.
        let last = dom.last_child(list).unwrap();
        dom.detach(last);
        engine.restyle(&dom);
        assert_eq!(style(&engine, &dom, 2, "padding-right"), "2px");
        assert_eq!(style(&engine, &dom, 1, "margin-left"), "7px");
        assert_eq!(engine.restyle_counts().0, 1, "styled from scratch once");
    }

    #[test]
    fn text_and_children_decide_empty() {
        let mut dom = parse_html(BODY, &Default::default()).dom;
        let mut engine = fresh_engine(&dom);
        let e = find(&dom, "e");
        let after = dom.next_sibling(e).unwrap();
        let check = |engine: &StyleEngine, empty: bool| {
            let height = ComputedStyle(engine.primary_style(e).unwrap()).get("height");
            let spacing = ComputedStyle(engine.primary_style(after).unwrap()).get("word-spacing");
            assert_eq!(height == "5px", empty, "{height}");
            assert_eq!(spacing == "3px", empty, "{spacing}");
        };
        check(&engine, true);
        let text = dom.create_text("t");
        dom.append_child(e, text);
        engine.restyle(&dom);
        check(&engine, false);
        dom.node_mut(text).kind = NodeKind::Text(String::new());
        engine.restyle(&dom);
        check(&engine, true);
        dom.node_mut(text).kind = NodeKind::Text("again".into());
        engine.restyle(&dom);
        check(&engine, false);
        dom.remove_subtree(text);
        engine.restyle(&dom);
        check(&engine, true);
        let child = dom.create_html_element("i", Vec::new());
        dom.append_child(e, child);
        engine.restyle(&dom);
        check(&engine, false);
        assert_eq!(engine.restyle_counts().0, 1, "styled from scratch once");
    }

    /// Sheets that come and go in [`sheets_coming_and_going_restyle_what_they_match`].
    const EXTRA_SHEETS: &[&str] = &[
        ".a { color: rgb(9, 1, 1) }",
        "#x { width: 33px }",
        "li { margin-left: 2px }",
        "* { letter-spacing: 2px }",
        "@media (min-width: 100px) { .b { margin-top: 9px } }",
        "p::before { content: \"y\"; color: rgb(3, 3, 3) }",
        ".s ~ span { background-color: rgb(4, 4, 4) }",
        ":root { --v: 5px } .c { padding-left: var(--v) }",
        "div:empty { height: 9px }",
        "dt:first-child { opacity: 0.75 }",
    ];

    #[test]
    fn sheets_coming_and_going_restyle_what_they_match() {
        let mut full_restyles = 0;
        for seed in 1..=10u64 {
            let mut dom = parse_html(BODY, &Default::default()).dom;
            let mut sheets: Vec<(u64, &str)> = vec![(1, SHEET)];
            let mut engine = fresh_engine_with(&dom, &sheets);
            let mut rng = Lcg(seed);
            let fixed = [find(&dom, "root"), find(&dom, "list"), find(&dom, "terms")];
            for step in 0..30 {
                let pick = rng.next(EXTRA_SHEETS.len());
                let sheet = (pick as u64 + 10, EXTRA_SHEETS[pick]);
                let what = match rng.next(5) {
                    // A sheet comes, at the end or somewhere in between.
                    0 | 1 if !sheets.contains(&sheet) => {
                        let at = rng.next(sheets.len() + 1);
                        sheets.insert(at, sheet);
                        format!("insert {} at {at}", sheet.1)
                    }
                    // One goes.
                    2 if sheets.len() > 1 => {
                        let gone = sheets.remove(rng.next(sheets.len()));
                        format!("remove {}", gone.1)
                    }
                    // Two swap places: the order of the sheets that stay
                    // changes.
                    3 if sheets.len() > 2 => {
                        let i = rng.next(sheets.len() - 1);
                        sheets.swap(i, i + 1);
                        "swap".to_string()
                    }
                    _ => format!("document: {}", mutate(&mut dom, &mut rng, fixed)),
                };
                let before = engine.restyle_counts().0;
                engine.set_author_stylesheets(&sheets);
                engine.restyle(&dom);
                full_restyles += engine.restyle_counts().0 - before;
                let expected = styles_of(&fresh_engine_with(&dom, &sheets), &dom);
                let actual = styles_of(&engine, &dom);
                assert_eq!(actual.len(), expected.len());
                for (a, e) in actual.iter().zip(&expected) {
                    assert_eq!(a, e, "seed {seed} step {step} ({what})");
                }
            }
        }
        // Only reordered sheets restyle from scratch.
        assert!(full_restyles < 60, "{full_restyles} restyles from scratch");
    }

    #[test]
    fn writes_that_change_nothing_restyle_nothing() {
        let mut dom = parse_html(BODY, &Default::default()).dom;
        let mut engine = fresh_engine(&dom);
        assert_eq!(engine.restyle_counts(), (1, 0));
        assert!(engine.take_restyled().full);
        let list = find(&dom, "list");
        // The same value again, and bookkeeping.
        dom.element_mut(list)
            .unwrap()
            .set_attr(attr_name("id"), "list");
        dom.set_custom_element_state(list, catpaw_dom::CustomElementState::Failed);
        engine.restyle(&dom);
        assert_eq!(engine.restyle_counts(), (1, 0));
        assert!(engine.is_fresh(&dom));
        // A class no selector looks at restyles nothing either.
        dom.element_mut(list)
            .unwrap()
            .set_attr(attr_name("class"), "unknown");
        engine.restyle(&dom);
        assert_eq!(engine.take_restyled(), Restyled::default());
        // One that matters restyles what it matches, and says so.
        dom.element_mut(list)
            .unwrap()
            .set_attr(attr_name("class"), "a");
        engine.restyle(&dom);
        assert_eq!(engine.restyle_counts().0, 1);
        let restyled = engine.take_restyled();
        assert!(!restyled.full);
        assert!(restyled.elements.contains(&list));
        assert_eq!(
            ComputedStyle(engine.primary_style(list).unwrap()).get("color"),
            "rgb(1, 0, 0)"
        );
    }
}
