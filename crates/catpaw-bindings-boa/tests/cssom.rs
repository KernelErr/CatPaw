//! The CSSOM: sheets of `<style>` elements, constructed sheets, their rules
//! and media, and that edits reach computed styles.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><html><head>
<style id="s" media="screen, print" title="main">p { color: red } @media (min-width: 1px) { b { display: block } }</style>
<style type="text/plain">p { color: blue }</style>
</head><body><p id="p">x</p><div id="d">y</div>
<script>
  var log = [];
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  function color(id) { return getComputedStyle(document.getElementById(id)).color; }
</script></body></html>"#;

fn settle(page: &mut BoaPage) {
    let report = page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
}

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| scripting::load_document(cx, html));
    settle(&mut page);
    page
}

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

#[test]
fn element_sheets_and_their_rules() {
    let mut page = load(FIXTURE);
    for (source, expected) in [
        (
            "var list = document.styleSheets; [list.length, String(list), list[0] === document.getElementById('s').sheet, list[0] === list.item(0), String(list[1]), document.getElementById('s').sheet === document.getElementById('s').sheet].join(' ')",
            "1 [object StyleSheetList] true true undefined true",
        ),
        (
            "var s = document.styleSheets[0]; [s.type, s.href, s.ownerNode.id, s.parentStyleSheet, s.title, s.disabled, s.media.mediaText, s.media.length, s.media[1], String(s.media), s.ownerRule].map(String).join('|')",
            "text/css|null|s|null|main|false|screen, print|2|print|screen, print|null",
        ),
        (
            "var r = s.cssRules; [r.length, String(r), r[0].cssText, r[0].type, r[0].selectorText, r[0] instanceof CSSStyleRule, r[1].type, r[1] instanceof CSSStyleRule, r[1] instanceof CSSRule, r[0] === r[0], r[0].parentStyleSheet === s, r[0].parentRule, s.rules === s.rules].map(String).join('|')",
            "2|[object CSSRuleList]|p { color: red; }|1|p|true|4|false|true|true|true|null|true",
        ),
        (
            "r[1].cssText",
            "@media (min-width: 1px) {\n  b { display: block; }\n}",
        ),
        ("color('p')", "rgb(255, 0, 0)"),
        (
            "s.insertRule('#p { color: green }', 1) + ' ' + s.cssRules.length + ' ' + color('p')",
            "1 3 rgb(0, 128, 0)",
        ),
        (
            "s.deleteRule(1); s.cssRules.length + ' ' + color('p')",
            "2 rgb(255, 0, 0)",
        ),
        (
            "attempt(function () { return s.insertRule('p {', 9); })",
            "IndexSizeError",
        ),
        (
            "attempt(function () { return s.insertRule('@nope x;'); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { return s.deleteRule(2); })",
            "IndexSizeError",
        ),
        (
            "attempt(function () { return s.replaceSync('a {}'); })",
            "NotAllowedError",
        ),
        (
            "s.addRule('#p', 'color: blue') + ' ' + s.cssRules[2].cssText + ' ' + color('p')",
            "-1 #p { color: blue; } rgb(0, 0, 255)",
        ),
        (
            "s.removeRule(2); s.disabled = true; s.cssRules.length + ' ' + color('p')",
            "2 rgb(0, 0, 0)",
        ),
        ("s.disabled = false; color('p')", "rgb(255, 0, 0)"),
        (
            "s.cssRules[0].selectorText = '#d'; s.cssRules[0].selectorText + ' ' + color('p') + ' ' + color('d')",
            "#d rgb(0, 0, 0) rgb(255, 0, 0)",
        ),
        (
            "s.cssRules[0].selectorText = '%%%'; s.cssRules[0].selectorText",
            "#d",
        ),
        (
            "s.media.mediaText = 'print'; s.media.appendMedium('Screen'); s.media.deleteMedium('print'); s.media.mediaText + ' ' + attempt(function () { s.media.deleteMedium('nope'); }) + ' ' + color('d')",
            "screen NotFoundError rgb(255, 0, 0)",
        ),
        // Changing the element's text replaces the sheet's rules.
        (
            "document.getElementById('s').textContent = 'div { color: teal }'; s.cssRules.length + ' ' + s.cssRules[0].selectorText + ' ' + color('d')",
            "1 div rgb(0, 128, 128)",
        ),
        (
            "var st = document.createElement('style'); st.textContent = 'p {}'; String(st.sheet) + ' ' + (document.head.appendChild(st), st.sheet !== null) + ' ' + document.styleSheets.length",
            "null true 2",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn constructed_sheets_are_adopted() {
    let mut page = load(FIXTURE);
    for (source, expected) in [
        (
            "var c = new CSSStyleSheet(); [String(c), c.ownerNode, c.href, c.cssRules.length, c.disabled, c.media.mediaText].map(String).join('|')",
            "[object CSSStyleSheet]|null|null|0|false|",
        ),
        (
            "c.replaceSync('@import url(x.css); #d { color: purple } #p { color: orange }'); c.cssRules.length + ' ' + color('d')",
            "2 rgb(0, 0, 0)",
        ),
        (
            "document.adoptedStyleSheets = [c]; document.adoptedStyleSheets.length + ' ' + (document.adoptedStyleSheets[0] === c) + ' ' + color('d') + ' ' + color('p')",
            "1 true rgb(128, 0, 128) rgb(255, 165, 0)",
        ),
        (
            "attempt(function () { document.adoptedStyleSheets = [document.styleSheets[0]]; })",
            "NotAllowedError",
        ),
        (
            "attempt(function () { return c.insertRule('@import url(y.css);'); })",
            "SyntaxError",
        ),
        (
            "var done = []; c.replace('#d { color: navy }').then(function (x) { done.push(x === c, color('d')); }); 0",
            "0",
        ),
        (
            "var m = new CSSStyleSheet({ media: 'print', disabled: true, baseURL: '/sub/' }); m.media.mediaText + ' ' + m.disabled + ' ' + attempt(function () { return new CSSStyleSheet({ baseURL: 'http://[bad' }); })",
            "print true NotAllowedError",
        ),
        (
            "var host = document.getElementById('d').attachShadow({ mode: 'open' }); host.adoptedStyleSheets = [c]; host.adoptedStyleSheets.length + ' ' + host.styleSheets.length",
            "1 0",
        ),
        (
            "document.adoptedStyleSheets = []; color('d')",
            "rgb(0, 0, 0)",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
    settle(&mut page);
    assert_eq!(eval(&mut page, "done.join(' ')"), "true rgb(0, 0, 128)");
}
