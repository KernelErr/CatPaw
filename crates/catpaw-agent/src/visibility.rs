//! Which nodes are rendered at all.

use catpaw_dom::{Dom, NodeId};

/// Answers the questions about a page that its markup alone cannot: how
/// it is styled, and what state its controls are in now. The engine
/// implements this over computed styles and live page state;
/// [`AttributeOracle`] approximates it from markup, which is all a
/// parse-only pipeline has.
pub trait StyleOracle {
    /// `display: none` (the element and its subtree generate no boxes).
    fn is_display_none(&self, dom: &Dom, id: NodeId) -> bool;
    /// `visibility: hidden | collapse` on the element itself.
    fn is_visibility_hidden(&self, dom: &Dom, id: NodeId) -> bool;

    /// The current value of a text control, when the page keeps one apart
    /// from the markup (what the user typed). `None`: read the markup.
    fn control_value(&self, _dom: &Dom, _id: NodeId) -> Option<String> {
        None
    }
    /// Whether a checkbox or radio button is checked now.
    fn is_checked(&self, _dom: &Dom, _id: NodeId) -> Option<bool> {
        None
    }
    /// Whether an `option` is selected now.
    fn is_option_selected(&self, _dom: &Dom, _id: NodeId) -> Option<bool> {
        None
    }
    /// The options a `select` shows as selected now.
    fn displayed_options(&self, _dom: &Dom, _select: NodeId) -> Option<Vec<NodeId>> {
        None
    }
    /// `cursor: pointer` on the element.
    fn is_pointer_cursor(&self, _dom: &Dom, _id: NodeId) -> bool {
        false
    }
    /// Whether the element's box is block-level (its text does not run on
    /// with the text around it). `None`: go by the element's kind.
    fn is_block_level(&self, _dom: &Dom, _id: NodeId) -> Option<bool> {
        None
    }
    /// Whether the element itself listens for clicks.
    fn has_activation_listener(&self, _dom: &Dom, _id: NodeId) -> bool {
        false
    }
    /// Whether the user typed into the element during a hand-off: what it
    /// holds shows as `***`.
    fn is_masked(&self, _dom: &Dom, _id: NodeId) -> bool {
        false
    }
}

/// Whether an element is an editing host the user typed into during a
/// hand-off: its text is theirs, and shows as `***`. (A field's value is
/// masked where values are read.)
pub fn masked_text(dom: &Dom, id: NodeId, oracle: &dyn StyleOracle) -> bool {
    dom.element(id).is_some_and(|el| {
        el.attr("contenteditable")
            .is_some_and(|v| !v.trim().eq_ignore_ascii_case("false"))
    }) && oracle.is_masked(dom, id)
}

/// Elements the HTML rendering section never displays, regardless of CSS.
pub fn is_never_rendered(local: &str) -> bool {
    matches!(
        local,
        "head"
            | "script"
            | "style"
            | "template"
            | "title"
            | "meta"
            | "link"
            | "base"
            | "noscript"
            | "param"
            | "source"
            | "track"
            | "datalist"
            | "noframes"
            | "area"
            | "rp"
    )
}

/// True when the element itself is hidden: `hidden` attribute, `aria-hidden`,
/// `display:none` per the oracle, or never-rendered element kinds.
pub fn is_hidden(dom: &Dom, id: NodeId, oracle: &dyn StyleOracle) -> bool {
    let Some(el) = dom.element(id) else {
        return false;
    };
    if is_never_rendered(&el.name.local) {
        return true;
    }
    if el.is_html()
        && &*el.name.local == "input"
        && el
            .attr("type")
            .is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
    {
        return true;
    }
    if el
        .attr("aria-hidden")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
    {
        return true;
    }
    // `visibility: hidden` elements are not interactable and Chromium drops
    // them from the accessibility tree; so do we (descendants that opt back
    // in with `visibility: visible` are rare enough to ignore for now).
    oracle.is_display_none(dom, id) || oracle.is_visibility_hidden(dom, id)
}

/// Approximates the style oracle from the `hidden` attribute and the inline
/// `style` attribute. Good enough for server-rendered pages; the style crate
/// replaces it once stylesheets are resolved.
#[derive(Debug, Default, Clone, Copy)]
pub struct AttributeOracle;

impl StyleOracle for AttributeOracle {
    fn is_display_none(&self, dom: &Dom, id: NodeId) -> bool {
        let Some(el) = dom.element(id) else {
            return false;
        };
        if el.has_attr("hidden") {
            return true;
        }
        el.attr("style")
            .is_some_and(|style| inline_declares(style, "display", &["none"]))
    }

    fn is_visibility_hidden(&self, dom: &Dom, id: NodeId) -> bool {
        dom.element(id)
            .and_then(|el| el.attr("style"))
            .is_some_and(|style| inline_declares(style, "visibility", &["hidden", "collapse"]))
    }
}

/// Whether an inline style attribute sets `property` to one of `values`
/// (last declaration wins, `!important` tolerated).
pub fn inline_declares(style: &str, property: &str, values: &[&str]) -> bool {
    let mut result = false;
    for decl in style.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case(property) {
            continue;
        }
        let value = value
            .trim()
            .trim_end_matches("!important")
            .trim()
            .to_ascii_lowercase();
        result = values.iter().any(|v| *v == value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use catpaw_dom::parse_html;

    #[test]
    fn inline_style_parsing() {
        assert!(inline_declares(
            "color:red; display : NONE !important",
            "display",
            &["none"]
        ));
        assert!(!inline_declares(
            "display:none;display:block",
            "display",
            &["none"]
        ));
        assert!(inline_declares(
            "visibility:collapse",
            "visibility",
            &["hidden", "collapse"]
        ));
    }

    #[test]
    fn hidden_detection() {
        let r = parse_html(
            "<div id=a hidden></div><div id=b style='display:none'></div><div id=c aria-hidden=true></div><input type=hidden id=d><p id=e>x</p>",
            &Default::default(),
        );
        let dom = &r.dom;
        let find = |id: &str| {
            dom.descendants(dom.document())
                .find(|&n| dom.attr(n, "id") == Some(id))
                .unwrap()
        };
        let oracle = AttributeOracle;
        for id in ["a", "b", "c", "d"] {
            assert!(is_hidden(dom, find(id), &oracle), "{id} should be hidden");
        }
        assert!(!is_hidden(dom, find("e"), &oracle));
    }
}
