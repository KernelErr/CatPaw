//! The style engine: device, stylist, stylesheets, and the restyle driver.

use std::sync::Once;
use std::sync::atomic::Ordering;

use catpaw_dom::{Dom, NodeId, NodeKind};
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
use style::properties::{ComputedValues, StyleBuilder, parse_style_attribute};
use style::queries::values::PrefersColorScheme;
use style::selector_parser::{PseudoElement, SnapshotMap};
use style::servo::media_features::PointerCapabilities;
use style::servo_arc::Arc;
use style::shared_lock::{SharedRwLock, StylesheetGuards};
use style::stylesheets::{
    AllowImportRules, CssRuleType, DocumentStyleSheet, Origin, Stylesheet, UrlExtraData,
};
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

/// Computes styles for one document.
pub struct StyleEngine {
    table: StyleTable,
    stylist: Stylist,
    snapshots: SnapshotMap,
    animations: style::animation::DocumentAnimationSet,
    url_data: UrlExtraData,
    quirks_mode: QuirksMode,
    /// The author stylesheets in cascade order, each with the key it was
    /// set under.
    author_sheets: Vec<(u64, DocumentStyleSheet)>,
    /// Styles resolved on demand, until the document next changes.
    resolved: UndisplayedStyleCache,
    /// The slots reflect the document as it is.
    slots_fresh: bool,
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
            slots_fresh: false,
        }
    }

    pub fn table(&self) -> &StyleTable {
        &self.table
    }

    /// Sets the document's quirks mode (from the parser) before styling.
    pub fn set_quirks_mode(&mut self, mode: catpaw_dom::QuirksMode) {
        self.quirks_mode = match mode {
            catpaw_dom::QuirksMode::Quirks => QuirksMode::Quirks,
            catpaw_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
            catpaw_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
        };
        self.stylist.set_quirks_mode(self.quirks_mode);
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
    /// Keys must be unique.
    pub fn set_author_stylesheets(&mut self, sheets: &[(u64, &str)]) {
        let unchanged = sheets.len() == self.author_sheets.len()
            && sheets
                .iter()
                .zip(&self.author_sheets)
                .all(|((key, _), (old, _))| key == old);
        if unchanged {
            return;
        }
        let lock = self.table.lock().clone();
        let guard = lock.read();
        let mut old: std::collections::HashMap<u64, DocumentStyleSheet> =
            self.author_sheets.drain(..).collect();
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
    }

    /// Tells the engine that the document changed: styles resolved on
    /// demand are forgotten.
    pub fn invalidate(&mut self) {
        self.resolved.clear();
        self.slots_fresh = false;
        self.table.clear_data();
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
        // A full restyle has the answer already.
        if let (None, Some(style)) = (pseudo, self.primary_style(id)) {
            return Some(ComputedStyle(style));
        }
        if !self.slots_fresh {
            self.ensure_slots(dom);
            self.slots_fresh = true;
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

    /// Creates slots for every connected element and refreshes the
    /// attribute-derived data Stylo reads during matching.
    pub fn ensure_slots(&mut self, dom: &Dom) {
        let document = dom.document();
        for id in dom.shadow_including_descendants(document) {
            let NodeKind::Element(el) = dom.kind(id) else {
                continue;
            };
            let url_data = &self.url_data;
            let quirks = self.quirks_mode;
            let lock = self.table.lock().clone();
            let (slot, _created) = self.table.ensure(id);
            slot.style_attribute = el.attr("style").map(|css| {
                let block = parse_style_attribute(css, url_data, None, quirks, CssRuleType::Style);
                Arc::new(lock.wrap(block))
            });
            slot.id_atom = el.id().map(style::Atom::from);
            slot.state = element_state(dom, id);
        }
    }

    /// Resolves styles for the whole document.
    pub fn restyle(&mut self, dom: &Dom) {
        self.ensure_slots(dom);
        let Some(root) = dom.child_elements(dom.document()).next() else {
            return;
        };

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
            if token.should_traverse() {
                let traversal = RecalcStyle { context };
                style::driver::traverse_dom(&traversal, token, None);
            }
            stylist.rule_tree().maybe_gc();
            thread_state::exit(ThreadState::LAYOUT);
        });
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
    /// are initial, and `display` is `block`.
    pub fn anonymous_block_style(&self, parent: &ComputedValues) -> Arc<ComputedValues> {
        let lock = self.table.lock().clone();
        let guard = lock.read();
        let guards = StylesheetGuards {
            author: &guard,
            ua_or_user: &guard,
        };
        self.stylist.style_for_anonymous::<CatNode>(
            &guards,
            &PseudoElement::ServoAnonymousBox,
            parent,
        )
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
    use catpaw_dom::parse_html;

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
}
