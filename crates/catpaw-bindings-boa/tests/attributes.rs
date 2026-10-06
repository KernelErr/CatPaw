//! Attributes as objects: `element.attributes`, `Attr`, and the methods
//! that take and return them.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r##"<!doctype html><body><div id="d" class="a b" data-x="1" TITLE="T"></div>
<svg id="s" xmlns:xlink="http://www.w3.org/1999/xlink"><use id="u" xlink:href="#a" viewBox="0 0 1 1"/></svg>
<script>
  var d = document.getElementById('d'), u = document.getElementById('u');
  var XLINK = 'http://www.w3.org/1999/xlink';
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  function list(element) { return Array.from(element.attributes, function (a) { return a.name + '=' + a.value; }).join(); }
</script>"##;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    let report = page.with_cx(|cx| {
        scripting::load_document(cx, html);
        event_loop::run(cx, &LoopLimits::default())
    });
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
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
fn an_element_lists_its_attributes() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "d.attributes instanceof NamedNodeMap && d.attributes === d.attributes",
                "true",
            ),
            (
                "d.attributes.length + ' ' + list(d)",
                "4 id=d,class=a b,data-x=1,title=T",
            ),
            (
                "d.attributes[0] === d.attributes[0] && d.attributes[0] === d.attributes.item(0) && d.attributes.id === d.getAttributeNode('id')",
                "true",
            ),
            (
                "d.attributes.class.value + '|' + d.attributes['data-x'].value",
                "a b|1",
            ),
            // Names are looked up in lowercase on HTML elements.
            (
                "d.attributes.getNamedItem('CLASS').name + ' ' + d.getAttributeNode('Title').value",
                "class T",
            ),
            (
                "[d.attributes.nope, d.attributes.getNamedItem('nope'), d.attributes[10], d.attributes.item(10)].join()",
                ",,,",
            ),
            (
                "'class' in d.attributes && 0 in d.attributes && !(10 in d.attributes)",
                "true",
            ),
            // Only the indices are enumerable.
            ("Object.keys(d.attributes).join()", "0,1,2,3"),
            (
                "var names = []; for (var i = d.attributes.length; i--;) names.push(d.attributes[i].name); names.join()",
                "title,data-x,class,id",
            ),
            (
                "var seen = []; for (var a of d.attributes) seen.push(a.localName); seen.join()",
                "id,class,data-x,title",
            ),
            // The list is live.
            (
                "d.setAttribute('new', 'n'); d.removeAttribute('title'); list(d)",
                "id=d,class=a b,data-x=1,new=n",
            ),
        ],
    );
}

#[test]
fn an_attr_is_a_view_of_its_attribute() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var a = d.attributes.class;
                 [a instanceof Attr, a instanceof Node, a.nodeType, a.nodeName, a.nodeValue, a.textContent, a.localName, a.prefix,
                  a.namespaceURI, a.specified, a.ownerElement === d, a.ownerDocument === document, a.parentNode, a.parentElement].join()",
                "true,true,2,class,a b,a b,class,,,true,true,true,,",
            ),
            ("d.className = 'c'; a.value", "c"),
            ("a.value = 'e f'; d.className", "e f"),
            ("a.nodeValue = 'g'; a.textContent = a.textContent + 'h'; d.getAttribute('class')", "gh"),
            // Without its attribute an Attr stands alone, with its last value.
            ("d.removeAttribute('class'); [a.ownerElement, a.value, d.attributes.length].join()", ",gh,3"),
            ("a.value = 'i'; d.hasAttribute('class') + ' ' + a.value", "false i"),
            // It can be put back, on any element.
            ("d.setAttributeNode(a) + ' ' + d.getAttribute('class') + ' ' + (a.ownerElement === d) + ' ' + (d.attributes.class === a)", "null i true true"),
            ("d.setAttributeNode(a) === a", "true"),
            ("attempt(function () { document.body.setAttributeNode(a); })", "InUseAttributeError"),
            ("d.removeAttributeNode(a) === a && !d.hasAttribute('class') && a.ownerElement", "null"),
            ("attempt(function () { d.removeAttributeNode(a); })", "NotFoundError"),
            ("document.body.setAttributeNode(a); document.body.className + ' ' + (a.ownerElement === document.body)", "i true"),
        ],
    );
}

#[test]
fn attributes_can_be_made_and_replaced() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var fresh = document.createAttribute('CLASS'); [fresh.name, fresh.value, fresh.ownerElement, fresh.ownerDocument === document].join()",
                "class,,,true",
            ),
            (
                "fresh.value = 'new'; var old = d.attributes.class, replaced = d.setAttributeNode(fresh);
                 [replaced === old, old.ownerElement, old.value, d.className, d.attributes.class === fresh].join()",
                "true,,a b,new,true",
            ),
            ("d.attributes.removeNamedItem('data-x').value + ' ' + d.hasAttribute('data-x')", "1 false"),
            ("attempt(function () { d.attributes.removeNamedItem('data-x'); })", "NotFoundError"),
            ("d.attributes.setNamedItem(old) === fresh && d.className", "a b"),
            ("attempt(function () { return document.createAttribute(''); })", "InvalidCharacterError"),
            ("attempt(function () { return document.createAttribute('a b'); })", "InvalidCharacterError"),
            // Documents hand out attributes of their own kind.
            ("document.implementation.createHTMLDocument('').createAttribute('Y').ownerDocument !== document", "true"),
            ("document.implementation.createDocument(null, 'r', null).createAttribute('Y').name", "Y"),
        ],
    );
}

#[test]
fn attributes_have_namespaces() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var x = u.getAttributeNodeNS(XLINK, 'href'); [x.name, x.localName, x.prefix, x.namespaceURI, x.value].join()",
                "xlink:href,href,xlink,http://www.w3.org/1999/xlink,#a",
            ),
            (
                "u.attributes['xlink:href'] === x && u.attributes.getNamedItemNS(XLINK, 'href') === x && u.getAttributeNode('xlink:href') === x",
                "true",
            ),
            ("u.getAttributeNodeNS(null, 'href') + ' ' + u.getAttributeNodeNS('', 'id').value", "null u"),
            // Names keep their case outside HTML.
            ("u.getAttributeNode('viewBox').name + ' ' + u.attributes.viewBox.value + ' ' + u.getAttributeNode('viewbox')", "viewBox 0 0 1 1 null"),
            ("x.value = '#b'; u.getAttributeNS(XLINK, 'href')", "#b"),
            (
                "var t = document.createAttributeNS(XLINK, 'xlink:title'); t.value = 'tip'; u.setAttributeNodeNS(t);
                 u.getAttributeNS(XLINK, 'title') + ' ' + (t.ownerElement === u)",
                "tip true",
            ),
            ("u.attributes.removeNamedItemNS(XLINK, 'title') === t && u.hasAttributeNS(XLINK, 'title')", "false"),
            ("attempt(function () { return document.createAttributeNS(null, 'a:b'); })", "NamespaceError"),
        ],
    );
}
