//! Documents other than the page's own: `document.implementation` and
//! `DOMParser`.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><body><div id="host"></div>
<script>
  var XHTML = 'http://www.w3.org/1999/xhtml', SVG = 'http://www.w3.org/2000/svg';
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  function parse(text, type) { return new DOMParser().parseFromString(text, type); }
</script>"#;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/app/index.html").unwrap(),
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
fn html_documents_can_be_made() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            ("var doc = document.implementation.createHTMLDocument('Hello'); doc !== document && doc instanceof Document", "true"),
            ("doc instanceof XMLDocument", "false"),
            ("doc.title + ' ' + doc.doctype.name", "Hello html"),
            (
                "doc.documentElement.outerHTML",
                "<html><head><title>Hello</title></head><body></body></html>",
            ),
            ("document.implementation.createHTMLDocument().head.childNodes.length", "0"),
            // A document without a window has a tree and little else.
            (
                "[doc.defaultView, doc.location, doc.URL, doc.readyState, doc.contentType, doc.compatMode, doc.characterSet].join()",
                ",,about:blank,complete,text/html,CSS1Compat,UTF-8",
            ),
            ("doc.hidden + ' ' + doc.visibilityState + ' ' + doc.hasFocus()", "true hidden false"),
            ("doc.cookie = 'a=b'; doc.cookie + '|' + doc.referrer + '|' + doc.currentScript", "||null"),
            ("doc.activeElement === doc.body && doc.body.baseURI", "about:blank"),
            ("doc.write('<p>written</p>'); doc.body.childNodes.length", "0"),
            ("getComputedStyle(doc.body).display", ""),
            ("doc.implementation === doc.implementation && doc.implementation !== document.implementation", "true"),
            ("document.implementation.hasFeature()", "true"),
            // Nodes know their document.
            ("doc.body.ownerDocument === doc && doc.ownerDocument === null && doc.body.getRootNode() === doc", "true"),
            ("doc.body.isConnected", "true"),
            ("var el = doc.createElement('DIV'); el.ownerDocument === doc && !el.isConnected && el.tagName", "DIV"),
            (
                "[doc.createTextNode('t'), doc.createComment('c'), doc.createDocumentFragment(), doc.createElementNS(SVG, 'g')]
                   .every(function (n) { return n.ownerDocument === doc; })",
                "true",
            ),
            ("document.createElement('p').ownerDocument === document", "true"),
            // Markup set there is parsed there, and scripts in it stay inert.
            (
                "doc.body.innerHTML = '<p id=\"x\">hi<script>window.ran = true<\\/script></p>';
                 var x = doc.getElementById('x'); x.ownerDocument === doc && doc.querySelector('#x') === x && typeof window.ran",
                "undefined",
            ),
            ("document.getElementById('x')", "null"),
            // Inserting a node adopts it.
            ("doc.body.appendChild(document.createElement('span')).ownerDocument === doc", "true"),
            (
                "document.body.appendChild(x); x.ownerDocument === document && x.firstChild.ownerDocument === document && document.getElementById('x') === x",
                "true",
            ),
            ("doc.getElementById('x') + ' ' + typeof window.ran", "null undefined"),
        ],
    );
}

#[test]
fn events_stay_in_their_document() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[(
            "var doc = document.implementation.createHTMLDocument(''), hits = [];
             window.addEventListener('ping', function () { hits.push('window'); });
             document.addEventListener('ping', function () { hits.push('page'); });
             doc.addEventListener('ping', function (e) { hits.push('other ' + (e.target === doc.body)); });
             doc.body.dispatchEvent(new Event('ping', { bubbles: true }));
             document.body.dispatchEvent(new Event('ping', { bubbles: true }));
             hits.join()",
            "other true,page,window",
        )],
    );
}

#[test]
fn xml_documents_can_be_made() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            ("var xml = document.implementation.createDocument('urn:x', 'x:root', null); xml instanceof XMLDocument && xml instanceof Document", "true"),
            (
                "var root = xml.documentElement; [root.tagName, root.localName, root.prefix, root.namespaceURI, xml.contentType].join()",
                "x:root,root,x,urn:x,application/xml",
            ),
            // Names keep their case, and elements have no namespace.
            ("var foo = xml.createElement('Foo'); foo.tagName + ' ' + foo.localName + ' ' + foo.namespaceURI", "Foo Foo null"),
            ("root.appendChild(foo); xml.getElementsByTagName('Foo').length + ' ' + xml.querySelector('Foo').ownerDocument.contentType", "1 application/xml"),
            ("document.implementation.createDocument(null, '', null).documentElement", "null"),
            (
                "var drawing = document.implementation.createDocument(SVG, 'svg', null); drawing.contentType + ' ' + (drawing.documentElement instanceof SVGSVGElement)",
                "image/svg+xml true",
            ),
            (
                "var xhtml = document.implementation.createDocument(XHTML, 'html', null), div = xhtml.createElement('DIV');
                 [xhtml.contentType, div.tagName, div.namespaceURI === XHTML, div instanceof HTMLElement].join()",
                "application/xhtml+xml,DIV,true,true",
            ),
            (
                "var type = document.implementation.createDocumentType('svg', 'pub', 'sys');
                 [type.name, type.publicId, type.systemId, type.ownerDocument === document].join()",
                "svg,pub,sys,true",
            ),
            (
                "var typed = document.implementation.createDocument(null, 'r', type); typed.doctype === type && type.ownerDocument === typed && typed.childNodes.length",
                "2",
            ),
            ("attempt(function () { return document.implementation.createDocumentType('a b', '', ''); })", "InvalidCharacterError"),
            ("attempt(function () { return document.implementation.createDocument(null, 'a b', null); })", "InvalidCharacterError"),
            // A prefix needs a namespace.
            ("attempt(function () { return document.implementation.createDocument(null, 'a:b', null); })", "NamespaceError"),
            ("attempt(function () { xml.write('x'); })", "InvalidStateError"),
            ("attempt(function () { xml.open(); })", "InvalidStateError"),
        ],
    );
}

#[test]
fn dom_parser_parses_html_without_running_it() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var parsed = parse('<!doctype html><title>T</title><p class=a>one<script>window.ran = 1<\\/script></p><noscript><b>n</b></noscript>', 'text/html');
                 [parsed.title, parsed.URL === document.URL, parsed.contentType, parsed.compatMode, parsed instanceof XMLDocument].join()",
                "T,true,text/html,CSS1Compat,false",
            ),
            ("parsed.querySelector('p.a').firstChild.data + ' ' + parsed.body.baseURI", "one https://example.test/app/index.html"),
            // Scripting is off there: `noscript` holds markup.
            ("parsed.querySelector('noscript b') !== null && typeof window.ran", "undefined"),
            // The scripts stay inert wherever they go.
            ("document.body.appendChild(parsed.querySelector('script')); typeof window.ran", "undefined"),
            ("parse('<p>x', 'text/html').compatMode", "BackCompat"),
            ("parse('', 'text/html').documentElement.outerHTML", "<html><head></head><body></body></html>"),
            ("attempt(function () { return parse('x', 'text/plain'); })", "TypeError"),
            ("attempt(function () { return new DOMParser().parseFromString('x'); })", "TypeError"),
        ],
    );
}

#[test]
fn dom_parser_parses_xml() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var feed = parse('<?xml version=\"1.0\" encoding=\"UTF-8\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"><entry id=\"e1\"><title>A &amp; B</title><Link href=\"/a\"/></entry><!-- c --></feed>', 'application/xml');
                 feed instanceof XMLDocument && feed.firstChild === feed.documentElement",
                "true",
            ),
            (
                "[feed.documentElement.nodeName, feed.documentElement.namespaceURI, feed.contentType, feed.URL === document.URL].join()",
                "feed,http://www.w3.org/2005/Atom,application/xml,true",
            ),
            ("feed.querySelector('entry title').textContent", "A & B"),
            ("feed.getElementById('e1') === feed.getElementsByTagName('entry')[0]", "true"),
            ("feed.getElementsByTagName('Link')[0].getAttribute('href') + ' ' + feed.getElementsByTagName('link').length", "/a 0"),
            ("feed.documentElement.lastChild.nodeType", "8"),
            (
                "var drawing = parse('<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"1\"/></svg>', 'image/svg+xml');
                 drawing.contentType + ' ' + (drawing.documentElement.firstChild instanceof SVGCircleElement)",
                "image/svg+xml true",
            ),
            ("parse('<a><b>text</b></a>', 'text/xml').documentElement.firstChild.tagName", "b"),
            // What is not well formed becomes a document that says so.
            (
                "var bad = parse('<a><b></a>', 'text/xml');
                 [bad.documentElement.nodeName, bad.documentElement.namespaceURI, bad.getElementsByTagName('parsererror').length, bad.childNodes.length].join()",
                "parsererror,http://www.mozilla.org/newlayout/xml/parsererror.xml,1,1",
            ),
            ("parse('', 'application/xml').documentElement.nodeName", "parsererror"),
            ("parse('just text', 'application/xml').documentElement.nodeName", "parsererror"),
            ("parse('<a/><b/>', 'application/xml').documentElement.nodeName", "parsererror"),
        ],
    );
}

#[test]
fn nodes_move_between_documents() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var other = parse('<p id=\"p\">text<b>bold</b></p><template id=\"t\"><i>in</i></template>', 'text/html'), p = other.getElementById('p');
                 var copy = document.importNode(p, true);
                 [copy !== p, copy.ownerDocument === document, copy.lastChild.ownerDocument === document, p.ownerDocument === other, copy.outerHTML].join()",
                "true,true,true,true,<p id=\"p\">text<b>bold</b></p>",
            ),
            ("document.importNode(p, false).childNodes.length + ' ' + document.importNode(p).childNodes.length", "0 0"),
            ("p.cloneNode(true).ownerDocument === other && p.cloneNode(false).ownerDocument === other", "true"),
            (
                "var adopted = document.adoptNode(p);
                 [adopted === p, p.ownerDocument === document, p.parentNode, p.firstChild.ownerDocument === document, other.getElementById('p')].join()",
                "true,true,,true,",
            ),
            ("attempt(function () { return document.importNode(other, true); })", "NotSupportedError"),
            ("attempt(function () { return document.adoptNode(other); })", "NotSupportedError"),
            // A template's contents travel with it.
            (
                "var t = document.adoptNode(other.getElementById('t')); t.content.ownerDocument === document && t.content.firstChild.ownerDocument === document",
                "true",
            ),
            // Documents can be cloned.
            (
                "var twin = other.cloneNode(true);
                 [twin !== other, twin instanceof Document, twin.body.ownerDocument === twin, twin.documentElement.outerHTML === other.documentElement.outerHTML, twin.URL === other.URL].join()",
                "true,true,true,true,true",
            ),
            ("other.cloneNode(false).childNodes.length + ' ' + other.cloneNode().contentType", "0 text/html"),
            ("document.cloneNode(true).body.childNodes.length > 0 && document.cloneNode(true).defaultView", "null"),
        ],
    );
}
