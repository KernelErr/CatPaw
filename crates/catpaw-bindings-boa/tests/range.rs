//! Live ranges: boundary points, comparison, contents, and following the
//! tree as it changes.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><body><div id="d"><p id="p1">Hello <b id="b">bold</b> world</p><p id="p2">Second</p></div>
<script>
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  function describe(r) { return (r.startContainer.id || r.startContainer.nodeName) + ':' + r.startOffset + '-' + (r.endContainer.id || r.endContainer.nodeName) + ':' + r.endOffset; }
</script></body></html>"#;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| scripting::load_document(cx, html));
    page
}

fn check(page: &mut BoaPage, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        let actual = match page.eval_to_string(source) {
            Ok(text) => text,
            Err(e) => format!("THROWN {e}"),
        };
        assert_eq!(actual, *expected, "{source}");
    }
}

#[test]
fn boundary_points_and_comparison() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var r = document.createRange(); [Object.prototype.toString.call(r), describe(r), r.collapsed, r.commonAncestorContainer === document, r instanceof AbstractRange, new Range().startContainer === document].join(' ')",
                "[object Range] #document:0-#document:0 true true true true",
            ),
            (
                "var p1 = document.getElementById('p1'), b = document.getElementById('b'), p2 = document.getElementById('p2'); r.setStart(p1.firstChild, 2); r.setEnd(p2.firstChild, 3); [describe(r), r.collapsed, r.commonAncestorContainer.id, r.toString()].join(' | ')",
                "#text:2-#text:3 | false | d | llo bold worldSec",
            ),
            (
                "r.setStart(p2.firstChild, 5); describe(r) + ' ' + r.collapsed",
                "#text:5-#text:5 true",
            ),
            (
                "r.selectNode(b); var s = document.createRange(); s.selectNodeContents(b); [describe(r), describe(s), r.compareBoundaryPoints(Range.START_TO_START, s), r.compareBoundaryPoints(Range.END_TO_END, s), s.compareBoundaryPoints(Range.START_TO_END, r), r.toString(), s.toString()].join(' ')",
                "p1:1-p1:2 b:0-b:1 -1 1 1 bold bold",
            ),
            (
                "r.setStartBefore(b); r.setEndAfter(p2); describe(r) + ' ' + attempt(function () { r.setStartBefore(document); }) + ' ' + attempt(function () { r.setStart(document.doctype, 0); }) + ' ' + attempt(function () { r.setStart(b, 5); }) + ' ' + attempt(function () { r.compareBoundaryPoints(7, s); })",
                "p1:1-d:2 InvalidNodeTypeError InvalidNodeTypeError IndexSizeError NotSupportedError",
            ),
            (
                "[r.isPointInRange(b.firstChild, 2), r.isPointInRange(p1.firstChild, 1), r.comparePoint(p1.firstChild, 0), r.comparePoint(b.firstChild, 1), r.comparePoint(document.body, document.body.childNodes.length), r.intersectsNode(b), r.intersectsNode(p1), r.intersectsNode(document.head), r.cloneRange().toString() === r.toString()].join(' ')",
                "true false -1 0 1 true true false true",
            ),
            (
                "r.collapse(true); r.collapsed + ' ' + describe(r) + ' ' + r.getBoundingClientRect().width",
                "true p1:1-p1:1 0",
            ),
            (
                "var sr = new StaticRange({ startContainer: p1, startOffset: 0, endContainer: p2, endOffset: 1 }); [String(sr), sr.startContainer.id, sr.endOffset, sr.collapsed, sr instanceof AbstractRange, sr instanceof Range, attempt(function () { new StaticRange({ startContainer: document.doctype, startOffset: 0, endContainer: p2, endOffset: 0 }); })].join(' ')",
                "[object StaticRange] p1 1 false true false InvalidNodeTypeError",
            ),
        ],
    );
}

#[test]
fn ranges_follow_the_tree() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var d = document.getElementById('d'), p1 = document.getElementById('p1'), p2 = document.getElementById('p2'); var r = document.createRange(); r.setStart(d, 1); r.setEnd(p2.firstChild, 3); d.insertBefore(document.createElement('hr'), p1); describe(r)",
                "d:2-#text:3",
            ),
            ("d.removeChild(d.firstChild); describe(r)", "d:1-#text:3"),
            (
                "p2.firstChild.insertData(1, 'XX'); var e1 = describe(r); p2.firstChild.deleteData(0, 4); var e2 = describe(r); p2.firstChild.data = 'S'; e1 + ' ' + e2 + ' ' + describe(r)",
                "d:1-#text:5 d:1-#text:1 d:1-#text:0",
            ),
            (
                "r.setEnd(p2.firstChild, 1); p2.firstChild.data = 'Second'; r.setEnd(p2.firstChild, 4); var tail = p2.firstChild.splitText(2); describe(r) + ' ' + (r.endContainer === tail)",
                "d:1-#text:2 true",
            ),
            (
                "r.selectNodeContents(p2); p2.remove(); describe(r) + ' ' + r.collapsed",
                "d:1-d:1 true",
            ),
            (
                "var gone = document.createRange(); gone.selectNode(p1); gone = null; d.appendChild(document.createElement('i')); 'survived'",
                "survived",
            ),
        ],
    );
}

#[test]
fn contents_are_cloned_extracted_and_deleted() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var d = document.getElementById('d'), p1 = document.getElementById('p1'), b = document.getElementById('b'), p2 = document.getElementById('p2'); var r = document.createRange(); r.setStart(p1.firstChild, 2); r.setEnd(p2.firstChild, 3); var c = r.cloneContents(); [String(c), c.childNodes.length, c.firstChild.outerHTML, c.lastChild.outerHTML, p1.outerHTML.length > 0, r.toString()].join(' | ')",
                "[object DocumentFragment] | 2 | <p id=\"p1\">llo <b id=\"b\">bold</b> world</p> | <p id=\"p2\">Sec</p> | true | llo bold worldSec",
            ),
            (
                "var x = r.extractContents(); [x.childNodes.length, x.firstChild.textContent, x.lastChild.textContent, d.innerHTML, describe(r), r.collapsed].join(' | ')",
                "2 | llo bold world | Sec | <p id=\"p1\">He</p><p id=\"p2\">ond</p> | d:1-d:1 | true",
            ),
            (
                "r.setStart(p1.firstChild, 1); r.setEnd(p2.firstChild, 2); r.deleteContents(); d.innerHTML + ' ' + describe(r)",
                "<p id=\"p1\">H</p><p id=\"p2\">d</p> d:1-d:1",
            ),
            (
                "var em = document.createElement('em'); em.textContent = 'new'; r.insertNode(em); d.innerHTML + ' ' + describe(r)",
                "<p id=\"p1\">H</p><em>new</em><p id=\"p2\">d</p> d:1-d:2",
            ),
            (
                "r.selectNodeContents(p1); var strong = document.createElement('strong'); r.surroundContents(strong); p1.innerHTML + ' ' + describe(r) + ' ' + r.toString()",
                "<strong>H</strong> p1:0-p1:1 H",
            ),
            (
                "r.setStart(p1.firstChild.firstChild, 0); r.setEnd(p2.firstChild, 1); attempt(function () { r.surroundContents(document.createElement('u')); })",
                "InvalidStateError",
            ),
            (
                "var cr = document.createRange(); cr.selectNodeContents(p2); var frag = cr.createContextualFragment('<li>a</li><b>c</b>'); [frag.childNodes.length, frag.firstChild.tagName, frag.lastChild.textContent].join(' ')",
                "2 LI c",
            ),
            (
                "var tr = document.createRange(); tr.selectNodeContents(document.querySelector('table') || p2); attempt(function () { var t = document.createElement('table'); document.body.appendChild(t); var r2 = document.createRange(); r2.selectNodeContents(t); return r2.createContextualFragment('<tr><td>x</td></tr>').firstChild.tagName; })",
                "TBODY",
            ),
        ],
    );
}
