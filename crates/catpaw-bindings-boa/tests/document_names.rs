//! The document's named properties (`document.myForm`), its element
//! collections, and the listeners that are passive by default.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><body>
<form name="login" id="loginForm"><input name="user"></form>
<img id="logo" name="brand" src="x.png"><img id="plain">
<a href="/one" name="first">one</a><a name="anchor-only">two</a><area href="/two">
<object id="plugin" name="player"></object>
<embed name="player">
<iframe name="frame"></iframe>
<script>var SCRIPTS = document.scripts.length;</script>
</body></html>"#;

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
fn the_document_has_collections_of_its_elements() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "[document.images.length, document.forms.length, document.links.length, document.anchors.length, document.embeds.length, document.plugins === document.embeds, document.scripts.length, SCRIPTS, document.applets.length, document.images === document.images, Object.prototype.toString.call(document.forms)].join(' ')",
                "2 1 2 2 1 true 1 1 0 true [object HTMLCollection]",
            ),
            (
                "document.body.appendChild(document.createElement('img')); document.images.length + ' ' + document.links.namedItem('first').textContent",
                "3 one",
            ),
        ],
    );
}

#[test]
fn named_elements_are_document_properties() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "[document.login === document.forms[0], document.loginForm, document.brand === document.images[0], document.logo === document.images[0], document.plain, document.first, document.frame.tagName, 'login' in document, 'nothing' in document].map(String).join(' ')",
                "true undefined true true undefined undefined IFRAME true false",
            ),
            (
                "var c = document.player; [Object.prototype.toString.call(c), c.length, c[0].tagName, c[1].tagName, c.namedItem('player') === c[0]].join(' ')",
                "[object HTMLCollection] 2 OBJECT EMBED true",
            ),
            (
                "document.plugin.tagName + ' ' + Object.keys(document).join(',')",
                "OBJECT login,logo,brand,plugin,player,frame",
            ),
            (
                "var img = document.images[0]; img.removeAttribute('name'); var a = [document.brand, document.logo]; img.setAttribute('name', 'fresh'); a.push(document.fresh === img, document.logo === img); img.remove(); a.push(document.fresh, document.logo); a.map(String).join(',')",
                "undefined,undefined,true,true,undefined,undefined",
            ),
            (
                r#"var f = document.createElement('form'); f.name = 'late'; document.body.appendChild(f); var r1 = document.late === f; var d = document.createElement('div'); d.innerHTML = '<img name="inner">'; document.body.appendChild(d); [r1, document.inner === d.firstChild, document.late.tagName].join(' ')"#,
                "true true FORM",
            ),
            (
                "var shadow = document.createElement('form'); shadow.name = 'createElement'; document.body.appendChild(shadow); var r = document.createElement === shadow; shadow.remove(); r + ' ' + (typeof document.createElement)",
                "true function",
            ),
            (
                r#"var other = new DOMParser().parseFromString('<form name="x"></form>', 'text/html'); [other.x.tagName, other.y, Object.getPrototypeOf(document) === Document.prototype, document instanceof Document, document instanceof EventTarget, document === document.documentElement.ownerDocument, document.body.ownerDocument.login.tagName].map(String).join(' ')"#,
                "FORM undefined true true true true FORM",
            ),
            (
                "document.expando = 1; var had = 'expando' in document; delete document.expando; had + ' ' + ('expando' in document) + ' ' + JSON.stringify(Object.getOwnPropertyDescriptor(document, 'login').enumerable)",
                "true false true",
            ),
        ],
    );
}

#[test]
fn scrolling_listeners_are_passive_by_default() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[(
            "function probe(target, type, options) { var prevented = null; function h(e) { e.preventDefault(); prevented = e.defaultPrevented; target.removeEventListener(type, h); } if (options === undefined) target.addEventListener(type, h); else target.addEventListener(type, h, options); var r = target.dispatchEvent(new Event(type, { cancelable: true })); return (prevented ? 'P' : 'p') + (r ? 'T' : 'F'); } [probe(window, 'wheel'), probe(document, 'touchstart'), probe(document.documentElement, 'mousewheel'), probe(document.body, 'touchmove'), probe(document.body, 'touchend'), probe(document.querySelector('form'), 'wheel'), probe(document, 'wheel', { passive: false }), probe(document, 'wheel', { passive: undefined }), probe(document.querySelector('form'), 'wheel', { passive: true }), probe(new EventTarget(), 'wheel')].join(' ')",
            "pT pT pT pT PF PF PF pT pT PF",
        )],
    );
}
