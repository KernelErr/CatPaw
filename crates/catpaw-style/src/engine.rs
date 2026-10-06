//! The style engine: device, stylist, stylesheets, and the restyle driver.

use std::sync::Once;
use std::sync::atomic::Ordering;

use catpaw_dom::{Dom, NodeId, NodeKind};
use selectors::matching::QuirksMode;
use style::context::{
    RegisteredSpeculativePainter, RegisteredSpeculativePainters, SharedStyleContext, StyleContext,
};
use style::device::Device;
use style::dom::TNode;
use style::global_style_data::GLOBAL_STYLE_DATA;
use style::media_queries::{MediaList, MediaType};
use style::properties::style_structs::Font;
use style::properties::{ComputedValues, parse_style_attribute};
use style::queries::values::PrefersColorScheme;
use style::selector_parser::SnapshotMap;
use style::servo::media_features::PointerCapabilities;
use style::servo_arc::Arc;
use style::shared_lock::{SharedRwLock, StylesheetGuards};
use style::stylesheets::{
    AllowImportRules, CssRuleType, DocumentStyleSheet, Origin, Stylesheet, UrlExtraData,
};
use style::stylist::Stylist;
use style::thread_state::{self, ThreadState};
use style::traversal::{DomTraversal, recalc_style_at};
use style::traversal_flags::TraversalFlags;
use style_dom::ElementState;
use url::Url;

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

/// Font metrics until the text crate provides real ones: ex/ch/cap/ic fall
/// back to Stylo's defaults, and generic families use 16px.
#[derive(Debug)]
struct PlaceholderFontMetrics;

impl style::device::servo::FontMetricsProvider for PlaceholderFontMetrics {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        _font: &Font,
        _base_size: style::values::computed::CSSPixelLength,
        _flags: style::values::computed::font::QueryFontMetricsFlags,
    ) -> style::font_metrics::FontMetrics {
        style::font_metrics::FontMetrics::default()
    }

    fn base_size_for_generic(
        &self,
        _generic: style::values::computed::font::GenericFontFamily,
    ) -> style::values::computed::Length {
        style::values::computed::Length::new(16.0)
    }
}

struct NoPainters;

impl RegisteredSpeculativePainters for NoPainters {
    fn get(&self, _name: &style::Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

fn set_prefs() {
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

fn make_device(options: &StyleOptions) -> Device {
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
        Box::new(PlaceholderFontMetrics),
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
    author_sheets: Vec<DocumentStyleSheet>,
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
        self.author_sheets.push(sheet);
    }

    pub fn author_sheet_count(&self) -> usize {
        self.author_sheets.len()
    }

    /// Creates slots for every connected element and refreshes the
    /// attribute-derived data Stylo reads during matching.
    pub fn ensure_slots(&mut self, dom: &Dom) {
        let document = dom.document();
        for id in dom.descendants(document) {
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
fn element_state(dom: &Dom, id: NodeId) -> ElementState {
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
}
