//! Selector queries for the DOM APIs: `querySelector(All)`, `matches`,
//! `closest`.
//!
//! These run Stylo's selector matching without any style data. Elements need
//! no style slot: pseudo-class state and ids are derived from the markup.

use catpaw_dom::{Dom, NodeId};
use selectors::SelectorList;
use style::dom::TDocument;
use style::dom_apis::{self, MayUseInvalidation, QueryAll, QueryFirst, QuerySelectorAllResult};
use style::selector_parser::{SelectorImpl, SelectorParser};
use style::servo_arc::Arc;
use style::shared_lock::SharedRwLock;
use style::stylesheets::UrlExtraData;
use url::Url;

use crate::node::{CatNode, with_style_context};
use crate::table::StyleTable;

thread_local! {
    /// An empty style table: queries never read style data.
    static QUERY_TABLE: StyleTable = StyleTable::new(SharedRwLock::new());
    static URL_DATA: UrlExtraData =
        UrlExtraData(Arc::new(Url::parse("about:blank").expect("valid URL")));
}

/// A parsed selector list.
pub struct Selectors(SelectorList<SelectorImpl>);

impl Selectors {
    /// Parses a selector list; `None` if it is not valid (callers throw a
    /// `SyntaxError`).
    pub fn parse(css: &str) -> Option<Self> {
        URL_DATA.with(|url_data| {
            SelectorParser::parse_author_origin_no_namespace(css, url_data)
                .ok()
                .map(Selectors)
        })
    }
}

fn with_query_context<R>(dom: &Dom, f: impl FnOnce() -> R) -> R {
    QUERY_TABLE.with(|table| with_style_context(dom, table, f))
}

/// Whether `element` matches any selector in the list.
pub fn matches(dom: &Dom, element: NodeId, selectors: &Selectors) -> bool {
    if !dom.is_element(element) {
        return false;
    }
    with_query_context(dom, || {
        let quirks = TDocument::quirks_mode(&CatNode::new(dom.document()));
        dom_apis::element_matches(&CatNode::new(element), &selectors.0, quirks)
    })
}

/// The nearest inclusive ancestor of `element` that matches.
pub fn closest(dom: &Dom, element: NodeId, selectors: &Selectors) -> Option<NodeId> {
    if !dom.is_element(element) {
        return None;
    }
    with_query_context(dom, || {
        let quirks = TDocument::quirks_mode(&CatNode::new(dom.document()));
        dom_apis::element_closest(CatNode::new(element), &selectors.0, quirks).map(|e| e.id)
    })
}

/// The first descendant of `root` (in tree order) that matches.
pub fn query_first(dom: &Dom, root: NodeId, selectors: &Selectors) -> Option<NodeId> {
    with_query_context(dom, || {
        let mut result: Option<CatNode> = None;
        dom_apis::query_selector::<CatNode, QueryFirst>(
            CatNode::new(root),
            &selectors.0,
            &mut result,
            MayUseInvalidation::No,
        );
        result.map(|e| e.id)
    })
}

/// Every descendant of `root` that matches, in tree order.
pub fn query_all(dom: &Dom, root: NodeId, selectors: &Selectors) -> Vec<NodeId> {
    with_query_context(dom, || {
        let mut results = QuerySelectorAllResult::<CatNode>::new();
        dom_apis::query_selector::<CatNode, QueryAll>(
            CatNode::new(root),
            &selectors.0,
            &mut results,
            MayUseInvalidation::No,
        );
        results.into_iter().map(|e| e.id).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use catpaw_dom::{HtmlParseOptions, parse_html};

    fn doc(html: &str) -> Dom {
        parse_html(html, &HtmlParseOptions::default()).dom
    }

    fn find(dom: &Dom, local: &str) -> NodeId {
        dom.descendants(dom.document())
            .find(|&n| dom.is_html_element(n, local))
            .unwrap()
    }

    #[test]
    fn queries_without_style_data() {
        let dom = doc(
            r#"<div id="a" class="x y"><p class="x">one</p><span><p id="deep">two</p></span></div>
               <input type="checkbox" checked><a href="/l">l</a><button disabled>b</button>"#,
        );
        let root = dom.document();
        let all = |css: &str| query_all(&dom, root, &Selectors::parse(css).unwrap()).len();
        assert_eq!(all("p"), 2);
        assert_eq!(all("div > p"), 1);
        assert_eq!(all("#deep"), 1);
        assert_eq!(all(".x"), 2);
        assert_eq!(all("div.x.y p"), 2);
        assert_eq!(all("input:checked"), 1);
        assert_eq!(all("a:link, button:disabled"), 2);
        assert_eq!(all("[class~=y]"), 1);
        assert_eq!(all("p:first-child"), 2);
        assert_eq!(all("p:nth-child(2)"), 0);
        assert!(Selectors::parse("p >").is_none());
        assert!(Selectors::parse("").is_none());

        let div = find(&dom, "div");
        let first = query_first(&dom, div, &Selectors::parse("p").unwrap()).unwrap();
        assert_eq!(dom.text_content(first), "one");
        // :scope refers to the query root.
        let scoped = query_all(&dom, div, &Selectors::parse(":scope > p").unwrap());
        assert_eq!(scoped.len(), 1);
        // The root itself is never part of the result.
        assert!(query_first(&dom, div, &Selectors::parse("div").unwrap()).is_none());
    }

    #[test]
    fn matches_and_closest() {
        let dom = doc(r#"<section class="s"><div><p id="p">x</p></div></section>"#);
        let p = find(&dom, "p");
        assert!(matches(&dom, p, &Selectors::parse("section p").unwrap()));
        assert!(!matches(&dom, p, &Selectors::parse("section > p").unwrap()));
        let section = closest(&dom, p, &Selectors::parse(".s").unwrap()).unwrap();
        assert!(dom.is_html_element(section, "section"));
        assert_eq!(closest(&dom, p, &Selectors::parse("p").unwrap()), Some(p));
        assert_eq!(
            closest(&dom, p, &Selectors::parse("article").unwrap()),
            None
        );
    }

    #[test]
    fn works_on_detached_subtrees() {
        let mut dom = doc("<p>attached</p>");
        let div = dom.create_html_element("div", Vec::new());
        let span = dom.create_html_element("span", vec![catpaw_dom::Attr::html("class", "k")]);
        dom.append_child(div, span);
        let found = query_all(&dom, div, &Selectors::parse("span.k").unwrap());
        assert_eq!(found, vec![span]);
        assert!(matches(&dom, span, &Selectors::parse("div > .k").unwrap()));
    }
}
