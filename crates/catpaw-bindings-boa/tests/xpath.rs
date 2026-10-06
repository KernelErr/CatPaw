//! `document.evaluate()`, `XPathEvaluator`, `XPathExpression` and
//! `XPathResult`, and the namespace lookups on `Node` they lean on.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r##"<!doctype html><html lang="en"><body id="b">
<div id="a" class="x"><p>one</p><p class="y">two</p><span hx-on:click="go()">s</span></div>
<ul><li>1</li><li>2</li><li>3</li></ul>
<svg xmlns="http://www.w3.org/2000/svg"><g xmlns:xlink="http://www.w3.org/1999/xlink"><a xlink:href="#t"/></g></svg>
<script>
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  function snapshot(expr, context) {
    var r = document.evaluate(expr, context || document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
    var out = [];
    for (var i = 0; i < r.snapshotLength; i++) { var n = r.snapshotItem(i); out.push(n.nodeName + (n.nodeType === 2 ? '=' + n.value : '')); }
    return out.join(',');
  }
</script></body></html>"##;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| scripting::load_document(cx, html));
    page
}

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

#[test]
fn evaluates_through_the_document_and_an_evaluator() {
    let mut page = load(FIXTURE);
    for (source, expected) in [
        ("snapshot('//p')", "P,P"),
        ("snapshot('//div/p[2]/text()')", "#text"),
        (
            "snapshot('.//*[@*[starts-with(name(), \"hx-on:\")]]')",
            "SPAN",
        ),
        ("snapshot('//span/@*')", "hx-on:click=go()"),
        ("snapshot('id(\"a\")/p[@class]')", "P"),
        ("snapshot('li', document.querySelector('ul'))", "LI,LI,LI"),
        (
            "var r = document.evaluate('count(//li)', document, null, XPathResult.NUMBER_TYPE, null); r.resultType + ' ' + r.numberValue",
            "1 3",
        ),
        (
            "var r = document.evaluate('string(//p[1])', document); r.resultType + ' ' + r.stringValue",
            "2 one",
        ),
        (
            "var r = document.evaluate('//li = 2', document); r.resultType + ' ' + r.booleanValue",
            "3 true",
        ),
        (
            "var r = document.evaluate('//li', document, null, XPathResult.FIRST_ORDERED_NODE_TYPE); r.resultType + ' ' + r.singleNodeValue.textContent",
            "9 1",
        ),
        (
            "var r = document.evaluate('//nothing', document, null, XPathResult.ANY_UNORDERED_NODE_TYPE); String(r.singleNodeValue)",
            "null",
        ),
        (
            "var it = document.evaluate('//li', document); var seen = []; var n; while ((n = it.iterateNext())) seen.push(n.textContent); it.resultType + ' ' + seen.join('') + ' ' + it.invalidIteratorState",
            "4 123 false",
        ),
        (
            "var it = document.evaluate('//li', document); it.iterateNext(); document.body.setAttribute('data-x', '1'); it.invalidIteratorState + ' ' + attempt(function () { return it.iterateNext(); })",
            "true InvalidStateError",
        ),
        (
            "var e = new XPathEvaluator().createExpression('//li[last()]'); var r = e.evaluate(document, XPathResult.STRING_TYPE); String(e) + ' ' + r.stringValue + ' ' + e.evaluate(document.body).resultType",
            "[object XPathExpression] 3 4",
        ),
        (
            "attempt(function () { return document.evaluate('//p[', document); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { return document.createExpression('foo('); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { return document.evaluate('//p', document, null, XPathResult.NUMBER_TYPE).stringValue; })",
            "TypeError",
        ),
        (
            "attempt(function () { return document.evaluate('1 + 1', document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE); })",
            "TypeError",
        ),
        (
            "attempt(function () { return document.evaluate('//p', document, null, 42); })",
            "NotSupportedError",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn prefixes_resolve_through_resolvers_and_nodes() {
    let mut page = load(FIXTURE);
    for (source, expected) in [
        (
            "attempt(function () { return document.evaluate('//s:g', document); })",
            "NamespaceError",
        ),
        ("snapshot('//s:g/s:a', document) ", "THROWN"),
        (
            "var r = document.evaluate('//s:g/s:a', document, function (p) { return p === 's' ? 'http://www.w3.org/2000/svg' : null; }, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE); r.snapshotLength + ' ' + r.snapshotItem(0).namespaceURI",
            "1 http://www.w3.org/2000/svg",
        ),
        (
            "var g = document.querySelector('g'); var r = document.evaluate('.//@xlink:href', g, document.createNSResolver(g), XPathResult.STRING_TYPE); r.stringValue + ' ' + (document.createNSResolver(g) === g)",
            "#t true",
        ),
        (
            "var r = document.evaluate('//svg:a', document, { lookupNamespaceURI: function (p) { return 'http://www.w3.org/2000/svg'; } }, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE); r.snapshotLength",
            "1",
        ),
        (
            "[document.lookupNamespaceURI(null), document.body.lookupNamespaceURI('xlink'), document.querySelector('g').lookupNamespaceURI('xlink'), document.querySelector('svg').lookupPrefix('http://www.w3.org/1999/xlink'), document.querySelector('g').lookupPrefix('http://www.w3.org/1999/xlink'), document.isDefaultNamespace('http://www.w3.org/1999/xhtml'), document.querySelector('svg').isDefaultNamespace('http://www.w3.org/2000/svg'), document.body.lookupPrefix(null)].map(String).join(' ')",
            "http://www.w3.org/1999/xhtml null http://www.w3.org/1999/xlink null xlink true true null",
        ),
    ] {
        let got = eval(&mut page, source);
        if expected == "THROWN" {
            assert!(got.starts_with("THROWN"), "{source}: {got}");
        } else {
            assert_eq!(got, expected, "{source}");
        }
    }
}
