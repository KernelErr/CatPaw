//! End-to-end tests: documents with scripts, run through the parser, the
//! bindings and the event loop.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_dom::to_html;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{ConsoleLevel, PageConfig, PageState, scripting};
use url::Url;

const URL: &str = "https://example.test/dir/page.html?q=1#frag";

/// Creates a page without loading anything into it.
fn blank() -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse(URL).unwrap(),
        PageConfig {
            time_origin_unix_ms: Some(1_700_000_000_000.0),
            ..PageConfig::default()
        },
    ));
    BoaPage::new(state).expect("page setup")
}

/// Loads `html` and runs the event loop until the page is idle.
fn load(html: &str) -> BoaPage {
    let mut page = blank();
    let report = page.with_cx(|cx| {
        scripting::load_document(cx, html);
        event_loop::run(cx, &LoopLimits::default())
    });
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
    page
}

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

fn body_html(page: &BoaPage) -> String {
    let dom = page.page().dom.borrow();
    let body = dom
        .descendants(dom.document())
        .find(|&n| dom.is_html_element(n, "body"))
        .expect("body");
    to_html(&dom, body, true)
}

fn console(page: &BoaPage, level: ConsoleLevel) -> Vec<String> {
    page.page()
        .console_messages()
        .into_iter()
        .filter(|m| m.level == level)
        .map(|m| m.text)
        .collect()
}

#[test]
fn the_global_object_is_the_window() {
    let mut page = blank();
    assert_eq!(eval(&mut page, "1 + 1"), "2");
    assert_eq!(
        eval(
            &mut page,
            "window === globalThis && self === window && window.window === window"
        ),
        "true"
    );
    assert_eq!(
        eval(&mut page, "typeof document + ' ' + document.nodeType"),
        "object 9"
    );
    assert_eq!(
        eval(&mut page, "Object.prototype.toString.call(window)"),
        "[object Window]"
    );
    assert_eq!(
        eval(
            &mut page,
            "window instanceof Window && window instanceof EventTarget"
        ),
        "true"
    );
    assert_eq!(eval(&mut page, "var declared = 5; window.declared"), "5");
    assert_eq!(eval(&mut page, "location.href"), URL);
    assert_eq!(eval(&mut page, "String(location)"), URL);
    assert_eq!(
        eval(
            &mut page,
            "location.search + location.hash + location.pathname"
        ),
        "?q=1#frag/dir/page.html"
    );
    assert_eq!(eval(&mut page, "origin"), "https://example.test");
    assert_eq!(
        eval(
            &mut page,
            "navigator.webdriver + ' ' + navigator.userAgent.startsWith('CatPaw/')"
        ),
        "true true"
    );
    assert_eq!(
        eval(&mut page, "innerWidth + 'x' + innerHeight"),
        "1280x720"
    );
    // Absent features are absent, not stubbed.
    assert_eq!(
        eval(
            &mut page,
            "typeof RTCPeerConnection + ' ' + ('serviceWorker' in navigator)"
        ),
        "undefined false"
    );
}

#[test]
fn interfaces_form_prototype_chains() {
    let mut page =
        load("<body><div id=d></div><input id=i><x-foo id=c></x-foo><blink id=u></blink>");
    let chain = "var b = document.body; [b instanceof HTMLBodyElement, b instanceof HTMLElement, b instanceof Element, b instanceof Node, b instanceof EventTarget, b instanceof HTMLDivElement].join()";
    assert_eq!(eval(&mut page, chain), "true,true,true,true,true,false");
    assert_eq!(
        eval(
            &mut page,
            "Object.getPrototypeOf(HTMLElement) === Element && Object.getPrototypeOf(HTMLDivElement.prototype) === HTMLElement.prototype"
        ),
        "true"
    );
    assert_eq!(
        eval(
            &mut page,
            "Object.prototype.toString.call(document.getElementById('d'))"
        ),
        "[object HTMLDivElement]"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('c').constructor.name + ' ' + document.getElementById('u').constructor.name"
        ),
        "HTMLElement HTMLUnknownElement"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.constructor.name + ' ' + document.createTextNode('x').constructor.name"
        ),
        "Document Text"
    );
    assert_eq!(
        eval(
            &mut page,
            "Node.ELEMENT_NODE + ' ' + document.body.TEXT_NODE + ' ' + Node.DOCUMENT_POSITION_CONTAINS"
        ),
        "1 3 8"
    );
    assert!(eval(&mut page, "new HTMLDivElement()").contains("Illegal constructor"));
    assert!(eval(&mut page, "Event('x')").contains("Please use the 'new' operator"));
    assert!(
        eval(
            &mut page,
            "Node.prototype.appendChild.call({}, document.body)"
        )
        .contains("Illegal invocation")
    );
    assert!(eval(&mut page, "document.body.appendChild('nope')").contains("not of type 'Node'"));
    // Wrappers are stable and can carry expandos.
    assert_eq!(
        eval(
            &mut page,
            "document.body.mark = 7; document.querySelector('body').mark + (document.body === document.body ? 1 : 0)"
        ),
        "8"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.body.classList === document.body.classList && document.body.childNodes === document.body.childNodes"
        ),
        "true"
    );
    // Accessors live on prototypes and are enumerable.
    assert_eq!(
        eval(
            &mut page,
            "'id' in Element.prototype && !document.body.hasOwnProperty('id') && Object.keys(Event.prototype).includes('type')"
        ),
        "true"
    );
}

#[test]
fn scripts_run_while_parsing_and_see_the_tree_so_far() {
    let mut page = load(
        r#"<!DOCTYPE html><title>T</title><body><div id="a">old</div>
<script>
  var seen = document.getElementById('late') === null;
  document.getElementById('a').textContent = 'new';
  var p = document.createElement('p');
  p.id = 'made';
  p.className = 'x y';
  document.body.appendChild(p);
</script><span id="late"></span>"#,
    );
    assert_eq!(eval(&mut page, "seen"), "true");
    assert_eq!(
        body_html(&page).replace('\n', ""),
        r#"<div id="a">new</div><script>  var seen = document.getElementById('late') === null;  document.getElementById('a').textContent = 'new';  var p = document.createElement('p');  p.id = 'made';  p.className = 'x y';  document.body.appendChild(p);</script><p id="made" class="x y"></p><span id="late"></span>"#
    );
    assert_eq!(
        eval(
            &mut page,
            "document.title + '|' + document.readyState + '|' + document.compatMode"
        ),
        "T|complete|CSS1Compat"
    );
}

#[test]
fn tasks_microtasks_and_timers_run_in_order() {
    let mut page = load(
        r#"<script>
  var log = [];
  setTimeout(function () { log.push('timeout 10'); }, 10);
  setTimeout(function (a, b) { log.push('timeout 0 ' + a + b); }, 0, 'x', 'y');
  var cancelled = setTimeout(function () { log.push('never'); }, 5);
  clearTimeout(cancelled);
  Promise.resolve().then(function () { log.push('promise'); });
  queueMicrotask(function () { log.push('microtask'); });
  var ticks = 0;
  var interval = setInterval(function () {
    log.push('tick ' + ++ticks);
    if (ticks === 2) clearInterval(interval);
  }, 20);
  requestAnimationFrame(function (t) { log.push('frame ' + (typeof t)); });
  setTimeout("log.push('string handler')", 1);
  log.push('sync');
</script>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join('; ')"),
        "sync; promise; microtask; timeout 0 xy; string handler; timeout 10; frame number; tick 1; tick 2"
    );
    // Virtual time jumped ahead instead of waiting.
    assert_eq!(
        eval(
            &mut page,
            "performance.now() >= 40 && performance.now() < 60"
        ),
        "true"
    );
    assert_eq!(eval(&mut page, "new Date().getFullYear()"), "2023");
}

#[test]
fn events_capture_bubble_and_cancel() {
    let mut page = load(
        r#"<body><div id="outer"><button id="inner" onclick="log.push('attr ' + event.type + ' ' + (this === event.currentTarget))">b</button></div>
<script>
  var log = [];
  var outer = document.getElementById('outer'), inner = document.getElementById('inner');
  function note(label) { return function (e) { log.push(label + ' ' + e.eventPhase + ' ' + (e.target === inner)); }; }
  window.addEventListener('click', note('window capture'), true);
  document.addEventListener('click', note('document capture'), { capture: true });
  outer.addEventListener('click', note('outer capture'), true);
  outer.addEventListener('click', note('outer bubble'));
  inner.addEventListener('click', note('inner'));
  inner.addEventListener('click', { handleEvent: function (e) { log.push('object ' + (this !== inner)); } });
  var once = 0;
  inner.addEventListener('click', function () { once++; }, { once: true });
  var same = note('dup');
  outer.addEventListener('dup', same); outer.addEventListener('dup', same);
  inner.click();
  inner.click();
</script>"#,
    );
    let log = eval(&mut page, "log.slice(0, 7).join('; ')");
    assert_eq!(
        log,
        "window capture 1 true; document capture 1 true; outer capture 1 true; inner 2 true; object true; attr click true; outer bubble 3 true"
    );
    assert_eq!(eval(&mut page, "log.length + ' ' + once"), "14 1");

    // stopPropagation, preventDefault and the return value of dispatchEvent.
    let cancel = r#"
      var hits = [];
      outer.addEventListener('custom', function () { hits.push('outer'); });
      inner.addEventListener('custom', function (e) { hits.push('inner ' + e.detail.n); e.stopPropagation(); e.preventDefault(); });
      var e = new CustomEvent('custom', { bubbles: true, cancelable: true, detail: { n: 4 } });
      var result = inner.dispatchEvent(e);
      [hits.join(), result, e.defaultPrevented, e.eventPhase, String(e.currentTarget), e.isTrusted, e instanceof Event].join(' ')
    "#;
    assert_eq!(
        eval(&mut page, cancel),
        "inner 4 false true 0 null false true"
    );
    assert_eq!(
        eval(
            &mut page,
            "outer.dispatchEvent(new Event('dup')); log.filter(function (l) { return l.startsWith('dup'); }).length"
        ),
        "1"
    );
    // Event handler IDL attributes.
    let handler = r#"
      var count = 0;
      inner.onclick = function (e) { count += 10; return false; };
      var ev = new Event('click', { cancelable: true });
      var ok = inner.dispatchEvent(ev);
      inner.onclick = null;
      inner.dispatchEvent(new Event('click'));
      count + ' ' + ok + ' ' + typeof inner.onclick + ' ' + (inner.onmouseover === null)
    "#;
    assert_eq!(eval(&mut page, handler), "10 false object true");
    assert!(eval(&mut page, "inner.dispatchEvent({})").contains("not of type 'Event'"));
}

#[test]
fn the_document_lifecycle_fires_in_order() {
    let mut page = load(
        r#"<script>
  var log = [document.readyState];
  document.addEventListener('readystatechange', function () { log.push('rsc ' + document.readyState); });
  document.addEventListener('DOMContentLoaded', function (e) { log.push('DCL ' + e.bubbles + ' ' + (document.currentScript === null)); });
  window.addEventListener('load', function (e) { log.push('load ' + (e.target === document || e.target === window)); });
  window.onload = function () { log.push('onload'); };
  log.push('current ' + (document.currentScript.tagName));
</script><body onload="log.push('body attr')">"#,
    );
    assert_eq!(
        eval(&mut page, "log.join('; ')"),
        "loading; current SCRIPT; rsc interactive; DCL true true; rsc complete; load true; onload"
    );
}

#[test]
fn document_write_inserts_at_the_parser_position() {
    let mut page = load(
        r#"<body><p>before</p><script>
  document.write('<b id="w">written</b>');
  document.write('<script>var inner = document.getElementById("w").textContent;<\/script>');
  document.writeln('<i>', 'two', '</i>');
</script><p>after</p>"#,
    );
    assert_eq!(eval(&mut page, "inner"), "written");
    let html = body_html(&page);
    let order: Vec<usize> = ["before", "written", "var inner", "two", "after"]
        .iter()
        .map(|needle| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing in {html}"))
        })
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "wrong order: {html}");
}

#[test]
fn scripts_inserted_by_script_run_once() {
    let mut page = load(
        r#"<body><script>
  var runs = 0;
  var s = document.createElement('script');
  s.textContent = 'runs++; var where = document.currentScript === s;';
  document.body.appendChild(s);
  var after = runs;
  document.body.removeChild(s);
  document.body.appendChild(s);
  var data = document.createElement('script');
  data.type = 'application/json';
  data.textContent = '{ not: javascript';
  document.body.appendChild(data);
  var holder = document.createElement('div');
  holder.innerHTML = '<script>runs += 100;<\/script>';
  document.body.appendChild(holder);
</script>"#,
    );
    assert_eq!(
        eval(&mut page, "after + ' ' + runs + ' ' + where"),
        "1 1 true"
    );
    assert!(console(&page, ConsoleLevel::Error).is_empty());
}

#[test]
fn dom_apis_work_from_script() {
    let mut page = load(
        r#"<body><ul id="list" class="a b" data-user-id="7"><li>one</li><li class="sel">two</li><li>three</li></ul>"#,
    );
    let run = |page: &mut BoaPage, source: &str| eval(page, source);
    assert_eq!(
        run(
            &mut page,
            "var list = document.getElementById('list'); list.children.length + ' ' + list.childNodes.length + ' ' + list.firstElementChild.textContent"
        ),
        "3 3 one"
    );
    assert_eq!(
        run(
            &mut page,
            "document.querySelectorAll('li').length + ' ' + document.querySelector('li.sel').textContent + ' ' + list.querySelector(':scope > li:last-child').textContent"
        ),
        "3 two three"
    );
    assert_eq!(
        run(
            &mut page,
            "var lis = list.getElementsByTagName('li'); var n = lis.length; list.appendChild(document.createElement('li')); n + ' ' + lis.length + ' ' + (lis[3] === list.lastChild) + ' ' + lis[9]"
        ),
        "3 4 true undefined"
    );
    assert_eq!(
        run(
            &mut page,
            "Array.from(document.querySelectorAll('li')).map(function (l) { return l.textContent; }).join('|')"
        ),
        "one|two|three|"
    );
    assert_eq!(
        run(
            &mut page,
            "var seen = []; document.querySelectorAll('li').forEach(function (l, i) { seen.push(i); }); for (var c of list.children) seen.push(c.tagName); seen.join()"
        ),
        "0,1,2,3,LI,LI,LI,LI"
    );
    // classList and dataset.
    assert_eq!(
        run(
            &mut page,
            "list.classList.add('c', 'a'); list.classList.remove('b'); list.classList.toggle('d'); list.className + '|' + list.classList.contains('c') + '|' + list.classList.length + '|' + list.classList[0] + '|' + [...list.classList].join('')"
        ),
        "a c d|true|3|a|acd"
    );
    assert!(run(&mut page, "list.classList.add('has space')").contains("InvalidCharacterError"));
    assert_eq!(
        run(
            &mut page,
            "list.dataset.userId + ' ' + ('userId' in list.dataset) + ' ' + Object.keys(list.dataset).join()"
        ),
        "7 true userId"
    );
    assert_eq!(
        run(
            &mut page,
            "list.dataset.fooBar = 'x'; delete list.dataset.userId; list.getAttribute('data-foo-bar') + ' ' + list.hasAttribute('data-user-id') + ' ' + JSON.stringify(list.dataset)"
        ),
        r#"x false {"fooBar":"x"}"#
    );
    // Attributes and reflection.
    assert_eq!(
        run(
            &mut page,
            "var a = document.createElement('a'); a.href = '/x?y'; a.setAttribute('TITLE', 't'); [a.href, a.getAttribute('href'), a.title, a.getAttributeNames().join(), a.hasAttribute('title'), a.tabIndex, a.hidden].join(' ')"
        ),
        "https://example.test/x?y /x?y t href,title true 0 false"
    );
    assert_eq!(
        run(
            &mut page,
            "var inp = document.createElement('input'); inp.setAttribute('type', 'checkbox'); inp.disabled = true; inp.maxLength = 5; [inp.outerHTML, inp.maxLength, inp.checked, inp.value].join(' ')"
        ),
        r#"<input type="checkbox" disabled="" maxlength="5"> 5 false on"#
    );
    assert_eq!(
        run(
            &mut page,
            "inp.disabled = false; inp.click(); inp.checked + ' ' + inp.hasAttribute('checked')"
        ),
        "true false"
    );
    // Markup in and out.
    assert_eq!(
        run(
            &mut page,
            "var d = document.createElement('div'); d.innerHTML = '<b>x</b><!--c--><table><tr><td>1'; d.innerHTML"
        ),
        "<b>x</b><!--c--><table><tbody><tr><td>1</td></tr></tbody></table>"
    );
    assert_eq!(
        run(
            &mut page,
            "d.firstChild.outerHTML = '<i>y</i>z'; d.insertAdjacentHTML('afterbegin', '<u>0</u>'); d.insertAdjacentText('beforeend', '!'); d.childNodes.length + d.textContent"
        ),
        "60yz1!"
    );
    assert_eq!(
        run(
            &mut page,
            "var t = document.createElement('template'); t.innerHTML = '<p>in</p>'; t.childNodes.length + ' ' + t.content.firstChild.tagName + ' ' + t.content.nodeType"
        ),
        "0 P 11"
    );
    // Tree manipulation and its errors.
    assert_eq!(
        run(
            &mut page,
            "var f = document.createDocumentFragment(); f.append('a', document.createElement('hr'), 'b'); d.replaceChildren(f); d.innerHTML + f.childNodes.length"
        ),
        "a<hr>b0"
    );
    assert_eq!(
        run(
            &mut page,
            "d.firstChild.after('1', '2'); d.lastChild.before(d.firstChild); d.querySelector('hr').replaceWith('|'); d.normalize(); d.textContent + d.childNodes.length"
        ),
        "12|ab1"
    );
    assert!(run(&mut page, "d.appendChild(d)").contains("HierarchyRequestError"));
    assert!(run(&mut page, "document.body.removeChild(d)").contains("NotFoundError"));
    assert!(
        run(
            &mut page,
            "document.appendChild(document.createElement('p'))"
        )
        .contains("HierarchyRequestError")
    );
    assert!(run(&mut page, "document.querySelector('p >')").contains("SyntaxError"));
    assert_eq!(
        run(
            &mut page,
            "try { document.createElement('1bad') } catch (e) { [e instanceof DOMException, e instanceof Error, e.name, e.code, e.constructor === DOMException].join() }"
        ),
        "true,true,InvalidCharacterError,5,true"
    );
    assert_eq!(
        run(
            &mut page,
            "var clone = list.cloneNode(true); clone.isEqualNode(list) + ' ' + clone.isSameNode(list) + ' ' + list.contains(list.firstChild) + ' ' + (list.compareDocumentPosition(list.firstChild) & Node.DOCUMENT_POSITION_CONTAINED_BY)"
        ),
        "true false true 16"
    );
    assert_eq!(
        run(
            &mut page,
            "var text = document.createTextNode('héllo wörld'); var tail = text.splitText(5); text.data + '|' + tail.data + '|' + text.length + '|' + tail.substringData(1, 3)"
        ),
        "héllo| wörld|5|wör"
    );
    assert_eq!(
        run(
            &mut page,
            "document.body.innerText.split('\\n').slice(0, 2).join('/') + ' ' + document.documentElement.tagName + ' ' + document.head.nodeName + ' ' + document.doctype"
        ),
        "one/two HTML HEAD null"
    );
}

#[test]
fn utility_apis_work() {
    let mut page = blank();
    assert_eq!(
        eval(
            &mut page,
            "var u = new URL('../a b?x=1&y=2#h', 'https://user:pw@example.com:8443/d/e/f'); [u.href, u.origin, u.host, u.pathname, u.search, u.hash, u.username].join(' ')"
        ),
        "https://user:pw@example.com:8443/d/a%20b?x=1&y=2#h https://example.com:8443 example.com:8443 /d/a%20b ?x=1&y=2 #h user"
    );
    assert_eq!(
        eval(
            &mut page,
            "u.searchParams.append('z', 'a&b'); u.searchParams.delete('x'); u.port = ''; u.search + ' ' + u.host + ' ' + (u.searchParams === u.searchParams)"
        ),
        "?y=2&z=a%26b example.com true"
    );
    assert_eq!(
        eval(
            &mut page,
            "u.search = '?q=1'; u.searchParams.get('q') + ' ' + u.searchParams.has('y') + ' ' + JSON.stringify(u) + ' ' + String(u).length"
        ),
        r#"1 false "https://user:pw@example.com/d/a%20b?q=1#h" 41"#
    );
    assert_eq!(
        eval(
            &mut page,
            "var p = new URLSearchParams('?b=2&a=1&b=3'); p.sort(); var out = []; for (var [k, v] of p) out.push(k + v); p.forEach(function (v, k) { out.push(k); }); out.join() + ' ' + p + ' ' + p.getAll('b') + ' ' + p.size + ' ' + [...p.keys()].join('')"
        ),
        "a1,b2,b3,a,b,b a=1&b=2&b=3 2,3 3 abb"
    );
    assert_eq!(
        eval(
            &mut page,
            "new URLSearchParams({ k: 'v', n: 1 }) + ' ' + new URLSearchParams([['a', 'b']]) + ' ' + URL.canParse('nope') + ' ' + URL.parse('nope')"
        ),
        "k=v&n=1 a=b false null"
    );
    assert!(eval(&mut page, "new URL('nope')").contains("TypeError"));

    assert_eq!(
        eval(
            &mut page,
            "btoa('héllo') + ' ' + atob('aOlsbG8=') + ' ' + atob(' aGk ')"
        ),
        "aOlsbG8= héllo hi"
    );
    assert!(eval(&mut page, "btoa('中')").contains("InvalidCharacterError"));
    assert!(eval(&mut page, "atob('***')").contains("InvalidCharacterError"));

    assert_eq!(
        eval(
            &mut page,
            "var bytes = new TextEncoder().encode('é中'); [bytes instanceof Uint8Array, bytes.length, Array.from(bytes).join('.')].join(' ')"
        ),
        "true 5 195.169.228.184.173"
    );
    assert_eq!(
        eval(
            &mut page,
            "new TextDecoder().decode(bytes) + new TextDecoder('gbk').decode(new Uint8Array([0xd6, 0xd0]).buffer) + ' ' + new TextDecoder('UTF-8').encoding"
        ),
        "é中中 utf-8"
    );
    assert!(eval(&mut page, "new TextDecoder('nope')").contains("RangeError"));
    assert!(
        eval(
            &mut page,
            "new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([255]))"
        )
        .contains("TypeError")
    );

    assert_eq!(
        eval(
            &mut page,
            "localStorage.setItem('a', 1); localStorage.b = 'two'; localStorage['c d'] = 3; [localStorage.length, localStorage.getItem('a'), localStorage.a, localStorage.key(1), Object.keys(localStorage).join(), 'b' in localStorage, String(localStorage.missing), String(localStorage.getItem('missing'))].join(' ')"
        ),
        "3 1 1 b a,b,c d true undefined null"
    );
    assert_eq!(
        eval(
            &mut page,
            "delete localStorage.a; localStorage.removeItem('b'); localStorage.length + ' ' + typeof localStorage.getItem + ' ' + (localStorage === window.localStorage) + ' ' + sessionStorage.length"
        ),
        "1 function true 0"
    );

    assert_eq!(
        eval(
            &mut page,
            "var o = { d: new Date(5), m: new Map([[1, { deep: [1, 2] }]]), s: 'x' }; o.self = o; var c = structuredClone(o); [c !== o, c.self === c, c.d.getTime(), c.m.get(1).deep.join(''), c.m.get(1) !== o.m.get(1)].join()"
        ),
        "true,true,5,12,true"
    );
    assert!(eval(&mut page, "structuredClone(function () {})").contains("DataCloneError"));
    assert!(eval(&mut page, "structuredClone(document)").contains("DataCloneError"));
}

#[test]
fn console_output_and_uncaught_errors_are_recorded() {
    let page = load(
        r#"<script>
  console.log('hello %s, you are %d', 'cat', 3.7, { extra: [1, 2] });
  console.warn('careful', document.createElement('br'), null, undefined);
  console.error(new RangeError('bad range'));
  console.assert(1 === 2, 'math');
  console.count(); console.count();
  alert('hi');
</script>
<script>thisDoesNotExist();</script>
<script>Promise.reject(new Error('lost'));</script>
<script>setTimeout(function () { throw new TypeError('later'); }, 0); var survived = true;</script>"#,
    );
    assert_eq!(
        console(&page, ConsoleLevel::Log),
        vec!["hello cat, you are 3 { extra: [ 1, 2 ] }"]
    );
    assert_eq!(
        console(&page, ConsoleLevel::Warn),
        vec!["careful <br> null undefined"]
    );
    let errors = console(&page, ConsoleLevel::Error);
    assert!(errors[0].starts_with("RangeError: bad range"), "{errors:?}");
    assert_eq!(errors[1], "Assertion failed: math");
    assert!(
        errors[2].starts_with("Uncaught ReferenceError"),
        "{errors:?}"
    );
    assert!(
        errors[3].starts_with("Uncaught (in promise) Error: lost"),
        "{errors:?}"
    );
    assert!(
        errors[4].starts_with("Uncaught TypeError: later"),
        "{errors:?}"
    );
    assert_eq!(errors.len(), 5);
    assert_eq!(
        console(&page, ConsoleLevel::Info),
        vec!["default: 1", "default: 2", "[alert] hi"]
    );
    assert_eq!(page.page().dialogs.borrow()[0].message, "hi");
    assert_eq!(page.page().errors.borrow().len(), 3);
}

#[test]
fn platform_objects_are_freed_when_script_drops_them() {
    let mut page = load("<body><div id=d></div>");
    let before = page.page().object_count();
    let made = eval(
        &mut page,
        r#"
      var kept = new Event('kept');
      var d = document.getElementById('d');
      d.addEventListener('x', function () {});
      for (var i = 0; i < 2000; i++) {
        d.dispatchEvent(new Event('x'));
        new URL('https://example.test/' + i);
        document.querySelectorAll('div');
      }
      i
    "#,
    );
    assert_eq!(made, "2000");
    assert!(page.page().object_count() >= before + 2000);
    page.with_cx(|cx| cx.script.collect_garbage());
    let after = page.page().object_count();
    assert!(
        after < before + 200,
        "{after} objects still alive (started with {before})"
    );
    // What script still holds is intact.
    assert_eq!(
        eval(&mut page, "kept.type + ' ' + (kept instanceof Event)"),
        "kept true"
    );
}

/// Runs the event loop until the page is idle again.
fn settle(page: &mut BoaPage) {
    let report = page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(report.stop, StopReason::Idle);
}

#[test]
fn history_tracks_same_document_navigations() {
    let mut page = load(
        r#"<script>
  var log = [];
  window.addEventListener('popstate', function (e) {
    log.push('pop ' + JSON.stringify(e.state) + ' ' + location.pathname + location.hash + ' ' + (e instanceof PopStateEvent));
  });
  window.onhashchange = function (e) {
    log.push('hash ' + e.oldURL.split('/').pop() + ' -> ' + e.newURL.split('/').pop());
  };
  history.replaceState({ n: 0 }, '', '/start');
  history.pushState({ n: 1 }, '', '/one?x=1');
  history.pushState({ n: 2 }, '', 'two#h');
  log.push(history.length + ' ' + JSON.stringify(history.state) + ' ' + location.href + ' ' + (document.URL === location.href));
  history.back();
  log.push('still ' + location.pathname);
</script>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join('; ')"),
        r#"3 {"n":2} https://example.test/two#h true; still /two; pop {"n":1} /one true"#
    );

    // A fragment navigation adds an entry (dropping the forward one) and
    // fires hashchange.
    assert_eq!(
        eval(
            &mut page,
            "log = []; location.hash = 'sec'; history.length + ' ' + location.href + ' ' + history.state"
        ),
        "3 https://example.test/one?x=1#sec null"
    );
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "log.join('; ')"),
        "hash one?x=1 -> one?x=1#sec"
    );

    assert_eq!(
        eval(&mut page, "log = []; history.go(-2); history.go(-5); 0"),
        "0"
    );
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "log.join('; ') + ' ' + location.pathname"),
        r#"pop {"n":0} /start true /start"#
    );
    assert_eq!(eval(&mut page, "log = []; history.forward(); 0"), "0");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "log.join('; ')"),
        r#"pop {"n":1} /one true"#
    );

    assert!(
        eval(
            &mut page,
            "history.pushState(null, '', 'https://other.example/')"
        )
        .contains("SecurityError")
    );
    assert_eq!(
        eval(
            &mut page,
            "history.scrollRestoration = 'manual'; history.scrollRestoration = 'bogus'; history.scrollRestoration"
        ),
        "manual"
    );
    assert_eq!(
        eval(
            &mut page,
            "history === window.history && history instanceof History"
        ),
        "true"
    );
    // A state object is cloned when stored.
    assert_eq!(
        eval(
            &mut page,
            "var st = { a: [1] }; history.replaceState(st, ''); st.a.push(2); JSON.stringify(history.state)"
        ),
        r#"{"a":[1]}"#
    );
    assert!(eval(&mut page, "history.pushState(function () {}, '')").contains("DataCloneError"));
}

#[test]
fn uncaught_errors_and_rejections_fire_events() {
    let page = load(
        r#"<script>
  var seen = [];
  window.onerror = function (message, source, line, column, error) {
    seen.push('onerror ' + message + ' | ' + (error instanceof RangeError) + ' | ' + typeof source + typeof line);
    return error.message === 'quiet';
  };
  window.addEventListener('error', function (e) {
    seen.push('listener ' + (e instanceof ErrorEvent) + ' ' + e.error.message + ' ' + e.cancelable);
  });
  window.addEventListener('unhandledrejection', function (e) {
    seen.push('rejection ' + e.reason + ' ' + (e.promise instanceof Promise) + ' ' + (e instanceof PromiseRejectionEvent));
    if (e.reason === 'claimed') e.preventDefault();
  });
</script>
<script>throw new RangeError('loud');</script>
<script>throw new RangeError('quiet');</script>
<script>Promise.reject('claimed'); Promise.reject('unclaimed');</script>
<script>console.info(seen.join('\n'));</script>"#,
    );
    assert_eq!(
        console(&page, ConsoleLevel::Info),
        vec![
            "onerror Uncaught RangeError: loud | true | stringnumber\n\
             listener true loud true\n\
             onerror Uncaught RangeError: quiet | true | stringnumber\n\
             listener true quiet true\n\
             rejection claimed true true\n\
             rejection unclaimed true true"
        ]
    );
    // Cancelled reports stay out of the console but are still counted.
    let errors = console(&page, ConsoleLevel::Error);
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].starts_with("Uncaught RangeError: loud"));
    assert_eq!(errors[1], "Uncaught (in promise) unclaimed");
    assert_eq!(page.page().errors.borrow().len(), 4);
}

#[test]
fn inline_styles_are_editable_through_the_cssom() {
    let mut page = load(r#"<body><div id="d" style="color: red; MARGIN: 1px 2px">x</div>"#);
    assert_eq!(
        eval(
            &mut page,
            "var d = document.getElementById('d'), s = d.style; [s.color, s.marginLeft, s['margin-top'], s.length, s[0], s.cssText].join('|')"
        ),
        "red|2px|1px|5|color|color: red; margin: 1px 2px;"
    );
    // Assignments are validated and normalized; invalid values are ignored.
    assert_eq!(
        eval(
            &mut page,
            "s.display = 'none'; s.backgroundColor = 'BLUE'; s.width = 'bogus'; s.notAProperty = 'x'; d.getAttribute('style')"
        ),
        "color: red; margin: 1px 2px; display: none; background-color: blue;"
    );
    assert_eq!(
        eval(
            &mut page,
            "s.setProperty('--x', ' 1 '); s.setProperty('height', '5px', 'important'); [s.getPropertyValue('--x'), s.getPropertyPriority('height'), s.removeProperty('color'), s.color].join('|')"
        ),
        "1|important|red|"
    );
    // Supported properties read as '' when unset; unknown names are absent.
    assert_eq!(
        eval(
            &mut page,
            "[('color' in s), ('nonsense' in s), s.nonsense === undefined, s.opacity === '', typeof s.setProperty, s === d.style, s instanceof CSSStyleDeclaration, Object.prototype.toString.call(s)].join(' ')"
        ),
        "true false true true function true true [object CSSStyleProperties]"
    );
    // The attribute is the source of truth, in both directions.
    assert_eq!(
        eval(
            &mut page,
            "d.setAttribute('style', 'color: green'); s.color + ' ' + s.length"
        ),
        "green 1"
    );
    assert_eq!(
        eval(&mut page, "d.style = 'top: 1px'; d.style.cssText"),
        "top: 1px;"
    );
    assert_eq!(
        eval(
            &mut page,
            "s.cssText = 'left: 2px; junk'; s.cssFloat = 'left'; d.getAttribute('style') + '|' + s.float + '|' + Object.keys(s).join()"
        ),
        "left: 2px; float: left;|left|0,1"
    );
    assert_eq!(
        eval(
            &mut page,
            "s.left = ''; s.float = null; d.getAttribute('style') + '|' + s.length"
        ),
        "|0"
    );
    assert_eq!(
        eval(
            &mut page,
            "var made = document.createElement('p'); made.style.fontSize = '12px'; made.outerHTML"
        ),
        r#"<p style="font-size: 12px;"></p>"#
    );
}

#[test]
fn svg_elements_have_their_interfaces() {
    let mut page = load(
        r##"<body><svg id="s" class="icon big" viewBox="0 0 10 10"><g id="g">
<a id="link" href="/x#y"><path id="p" d="M0 0"/></a>
<use id="u" xlink:href="#sym"/><text id="t">hi<tspan id="ts">x</tspan></text>
<foreignObject id="fo"><div id="inside"></div></foreignObject><unknown id="unk"/></g></svg>
<div id="d"></div>
<script>
  var $ = function (id) { return document.getElementById(id); };
  var s = $('s'), p = $('p'), link = $('link'), u = $('u');
  var XLINK = 'http://www.w3.org/1999/xlink';
</script>"##,
    );
    for (source, expected) in [
        (
            "s instanceof SVGSVGElement && s instanceof SVGGraphicsElement && s instanceof SVGElement && s instanceof Element",
            "true",
        ),
        ("s instanceof HTMLElement", "false"),
        (
            "Object.getPrototypeOf(SVGSVGElement.prototype) === SVGGraphicsElement.prototype",
            "true",
        ),
        (
            "p instanceof SVGPathElement && p instanceof SVGGeometryElement",
            "true",
        ),
        (
            "$('g').constructor.name + ' ' + $('fo').constructor.name",
            "SVGGElement SVGForeignObjectElement",
        ),
        ("$('inside') instanceof HTMLDivElement", "true"),
        (
            "$('t') instanceof SVGTextPositioningElement && $('t') instanceof SVGTextContentElement && $('ts') instanceof SVGTSpanElement",
            "true",
        ),
        ("$('unk').constructor === SVGElement", "true"),
        (
            "document.createElementNS('http://www.w3.org/2000/svg', 'circle') instanceof SVGCircleElement",
            "true",
        ),
        (
            "document.createElement('svg') instanceof HTMLUnknownElement",
            "true",
        ),
        // String attributes are objects on SVG elements.
        (
            "typeof s.className + ' ' + s.className.baseVal + '|' + s.className.animVal",
            "object icon big|icon big",
        ),
        (
            "s.className === s.className && s.className instanceof SVGAnimatedString",
            "true",
        ),
        (
            "s.className.baseVal = 'small'; s.getAttribute('class') + ' ' + s.classList.contains('small')",
            "small true",
        ),
        ("typeof $('d').className", "string"),
        ("link instanceof SVGAElement && link.href.baseVal", "/x#y"),
        (
            "u.href.baseVal + ' ' + u.hasAttribute('href')",
            "#sym false",
        ),
        (
            "u.href.baseVal = '#other'; u.getAttributeNS(XLINK, 'href') + ' ' + u.hasAttribute('href')",
            "#other false",
        ),
        ("link.href.baseVal = '/z'; link.getAttribute('href')", "/z"),
        (
            "p.ownerSVGElement === s && p.viewportElement === s && s.ownerSVGElement",
            "null",
        ),
        // What SVG elements share with HTML elements.
        (
            "s.style.fill = 'red'; s.getAttribute('style')",
            "fill: red;",
        ),
        (
            "s.dataset.kind = 'icon'; s.getAttribute('data-kind')",
            "icon",
        ),
        ("typeof s.focus + ' ' + ('onclick' in s)", "function true"),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn hyperlinks_take_their_urls_apart() {
    let mut page = load(
        r##"<body><a id="a" href="../docs/guide.html?q=1#top">The <b>guide</b></a>
<a id="abs" href="https://user:pw@other.test:8443/p/a/t/h?x=y#z">x</a><a id="none">n</a>
<script>
  var a = document.getElementById('a'), abs = document.getElementById('abs'), none = document.getElementById('none');
</script>"##,
    );
    for (source, expected) in [
        ("a.href", "https://example.test/docs/guide.html?q=1#top"),
        ("String(a)", "https://example.test/docs/guide.html?q=1#top"),
        (
            "[a.protocol, a.host, a.hostname, a.port, a.pathname, a.search, a.hash, a.origin].join(' ')",
            "https: example.test example.test  /docs/guide.html ?q=1 #top https://example.test",
        ),
        (
            "[abs.username, abs.password, abs.host, abs.hostname, abs.port, abs.pathname].join(' ')",
            "user pw other.test:8443 other.test 8443 /p/a/t/h",
        ),
        // Without an `href` there is no URL.
        (
            "[none.href, none.protocol, none.host, none.pathname, none.search, none.hash, none.origin].join('|')",
            "|:|||||",
        ),
        ("none.pathname = '/x'; none.hasAttribute('href')", "false"),
        // Writes go back to the attribute, as an absolute URL.
        (
            "a.pathname = '/api/'; a.getAttribute('href')",
            "https://example.test/api/?q=1#top",
        ),
        (
            "a.search = 'r=2'; a.hash = ''; a.href",
            "https://example.test/api/?r=2",
        ),
        (
            "a.hostname = 'docs.test'; a.port = '8080'; a.protocol = 'http'; a.href",
            "http://docs.test:8080/api/?r=2",
        ),
        (
            "a.host = 'h.test:1'; a.username = 'u'; a.password = 'p'; a.href",
            "http://u:p@h.test:1/api/?r=2",
        ),
        (
            "a.setAttribute('href', '#frag'); a.hash + ' ' + a.pathname",
            "#frag /dir/page.html",
        ),
        ("a.text + '|' + abs.text", "The guide|x"),
        (
            "a.text = 'new <text>'; a.innerHTML + ' ' + a.childNodes.length",
            "new &lt;text&gt; 1",
        ),
        (
            "a.target = '_blank'; a.rel = 'noopener'; a.getAttribute('target') + ' ' + a.relList.contains('noopener')",
            "_blank true",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn crypto_hands_out_random_values() {
    let mut page = load(
        r#"<script>
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
</script>"#,
    );
    for (source, expected) in [
        ("crypto instanceof Crypto && crypto === window.crypto && typeof crypto.subtle", "object"),
        (
            "var bytes = new Uint8Array(32), same = crypto.getRandomValues(bytes); same === bytes && bytes.some(function (b) { return b !== 0; })",
            "true",
        ),
        // Only the part of the buffer the array covers is filled.
        (
            "var whole = new Uint8Array(64), part = new Uint16Array(whole.buffer, 16, 8); crypto.getRandomValues(part);
             whole.slice(0, 16).every(function (b) { return b === 0; }) && whole.slice(32).every(function (b) { return b === 0; }) &&
             whole.slice(16, 32).some(function (b) { return b !== 0; })",
            "true",
        ),
        ("crypto.getRandomValues(new Uint8Array(0)).length", "0"),
        ("crypto.getRandomValues(new BigUint64Array(2)).length + ' ' + crypto.getRandomValues(new Int32Array(4)).length", "2 4"),
        ("crypto.getRandomValues(new Uint8Array(65536)).length", "65536"),
        ("attempt(function () { crypto.getRandomValues(new Uint8Array(65537)); })", "QuotaExceededError"),
        ("attempt(function () { crypto.getRandomValues(new Float32Array(4)); })", "TypeMismatchError"),
        ("attempt(function () { crypto.getRandomValues(new DataView(new ArrayBuffer(4))); })", "TypeError"),
        ("attempt(function () { crypto.getRandomValues([1, 2]); })", "TypeError"),
        ("attempt(function () { crypto.getRandomValues(); })", "TypeError"),
        ("attempt(function () { Crypto.prototype.getRandomValues.call({}, new Uint8Array(1)); })", "TypeError"),
        (
            "var id = crypto.randomUUID(); /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id) && id !== crypto.randomUUID()",
            "true",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn every_html_element_has_its_interface() {
    let mut page = load(
        "<body><dialog id=d open></dialog><canvas id=c width=40></canvas><video id=v></video><blockquote id=q cite='/src'></blockquote><progress id=p max=10 value=3></progress>",
    );
    for (source, expected) in [
        (
            "var d = document.getElementById('d'); d instanceof HTMLDialogElement && d.open",
            "true",
        ),
        (
            "d.open = false; d.hasAttribute('open') + ' ' + typeof d.showModal",
            "false undefined",
        ),
        (
            "var c = document.getElementById('c'); c instanceof HTMLCanvasElement && c.width + ' ' + c.height + ' ' + typeof c.getContext",
            "40 150 function",
        ),
        (
            "var v = document.getElementById('v'); v instanceof HTMLVideoElement && v instanceof HTMLMediaElement && v instanceof HTMLElement",
            "true",
        ),
        (
            "document.getElementById('q').cite",
            "https://example.test/src",
        ),
        (
            "var p = document.getElementById('p'); p instanceof HTMLProgressElement && p.getAttribute('max')",
            "10",
        ),
        (
            "document.createElement('bgsound') instanceof HTMLUnknownElement",
            "true",
        ),
        (
            "document.createElement('abbr').constructor.name + ' ' + document.createElement('xmp').constructor.name",
            "HTMLElement HTMLPreElement",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn document_domain_is_the_host() {
    let mut page = load(
        "<script>function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }</script>",
    );
    for (source, expected) in [
        ("document.domain", "example.test"),
        (
            "attempt(function () { document.domain = 'example.test'; return document.domain; })",
            "example.test",
        ),
        (
            "attempt(function () { document.domain = 'test'; })",
            "SecurityError",
        ),
        ("document.implementation.createHTMLDocument('').domain", ""),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn data_urls_are_fetched_without_a_network() {
    let mut page = load(
        r#"<script>var log = [];</script>
<script src="data:text/javascript;base64,bG9nLnB1c2goJ2Jhc2U2NCBzY3JpcHQnKTs="></script>
<script src="data:text/javascript,log.push('plain%20script')"></script>
<script type="module" src="data:text/javascript,log.push('module')"></script>
<script>
  fetch('data:text/plain;charset=utf-8,hi%20there').then(function (r) { return r.text(); }).then(function (t) { log.push('fetched ' + t); });
  fetch('data:,bare').then(function (r) { log.push(r.headers.get('content-type')); return r.text(); }).then(function (t) { log.push(t); });
</script>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join(' / ')"),
        "base64 script / plain script / module / fetched hi there / text/plain;charset=US-ASCII / bare"
    );
}

#[test]
fn css_namespace_and_element_factories() {
    let mut page = load(
        "<script>function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }</script>",
    );
    for (source, expected) in [
        (
            "CSS.supports('display', 'grid') + ' ' + CSS.supports('display', 'nope') + ' ' + CSS.supports('(display: flex) and (gap: 1px)') + ' ' + CSS.supports('nonsense')",
            "true false true false",
        ),
        (
            "CSS.escape('1a.b') + ' ' + String(CSS)",
            r"\31 a\.b [object CSS]",
        ),
        (
            "var img = new Image(10, 20); img instanceof HTMLImageElement && img.tagName + ' ' + img.getAttribute('width') + ' ' + img.getAttribute('height')",
            "IMG 10 20",
        ),
        (
            "new Image().hasAttribute('width') + ' ' + (Image.prototype === HTMLImageElement.prototype)",
            "false true",
        ),
        ("attempt(function () { return Image(); })", "TypeError"),
        (
            "var a = new Audio('/s.mp3'); a instanceof HTMLAudioElement && a.getAttribute('src') + ' ' + a.getAttribute('preload')",
            "/s.mp3 auto",
        ),
        (
            "var o = new Option('Text', 'v', true); o instanceof HTMLOptionElement && o.textContent + ' ' + o.getAttribute('value') + ' ' + o.hasAttribute('selected')",
            "Text v true",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

/// Engine behaviour the patched Boa crates in `vendor/` fix; this fails
/// again if a patch is dropped before a release has it.
#[test]
fn vendored_engine_fixes_hold() {
    let mut page = load("");
    for (source, expected) in [
        (
            "class A { constructor() { this.x = 5 } f() { return (() => () => this.x)()(); } g() { return (() => () => () => this)()()() === this; } }; new A().f() + ' ' + new A().g()",
            "5 true",
        ),
        (
            "class P { #m = 1; get #w() { return this.#m } f() { return [1].map(() => { const o = () => this.#w; return o(); }); } }; String(new P().f())",
            "1",
        ),
        (
            "var obj = { x: 9, f() { return (() => () => this.x)()(); } }; obj.f()",
            "9",
        ),
        // A body var named after a parameter, in a function with a default.
        (
            "function f(a, b = 1) { var x = b; var b = 5; return x; }; f(1, 2) + ' ' + f(1)",
            "2 1",
        ),
        (
            "function g(a, b = 1) { var h = () => b; var b = 5; return h(); }; g(1, 2)",
            "5",
        ),
        // Date.parse takes what browsers take.
        (
            "['2025-10-01T12:34:56.789123+00:00', '2025-10-01 12:34:56Z', '7 Oct 2026 10:00:00 GMT', 'Oct 7, 2026 10:30 PM UTC', '2026-10-07T10:00:00.000+0000', '10/07/2026 10:00 GMT'].map(function (s) { return new Date(s).toISOString(); }).join(' ') + ' ' + isNaN(new Date('nonsense'))",
            "2025-10-01T12:34:56.789Z 2025-10-01T12:34:56.000Z 2026-10-07T10:00:00.000Z 2026-10-07T22:30:00.000Z 2026-10-07T10:00:00.000Z 2026-10-07T10:00:00.000Z true",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn beacons_go_out_in_the_background() {
    let mut page = load("");
    assert_eq!(
        eval(
            &mut page,
            "navigator.sendBeacon('/collect', 'a=1') + ' ' + navigator.sendBeacon('/collect') + ' ' + navigator.sendBeacon('/collect', new URLSearchParams('b=2'))"
        ),
        "true true true"
    );
    assert_eq!(
        eval(
            &mut page,
            "try { navigator.sendBeacon('http://[bad'); } catch (e) { e.name }"
        ),
        "TypeError"
    );
}

#[test]
fn subtle_crypto_digests() {
    let mut page = load("<script>var log = [];</script>");
    assert_eq!(
        eval(
            &mut page,
            "var hex = function (b) { return Array.from(new Uint8Array(b)).map(function (x) { return x.toString(16).padStart(2, '0'); }).join(''); }; var data = new TextEncoder().encode('abc'); Promise.all([crypto.subtle.digest('SHA-1', data), crypto.subtle.digest({ name: 'sha-256' }, data), crypto.subtle.digest('SHA-384', data.buffer), crypto.subtle.digest('SHA-512', data)]).then(function (r) { log.push(r.map(hex).map(function (h) { return h.slice(0, 16); }).join(' ')); }); crypto.subtle.digest('MD5', data).catch(function (e) { log.push(e.name); }); crypto.subtle.digest(5, data).catch(function (e) { log.push(e.name); }); (crypto.subtle === crypto.subtle) + ' ' + String(crypto.subtle) + ' ' + typeof crypto.subtle.encrypt"
        ),
        "true [object SubtleCrypto] undefined"
    );
    page.with_cx(|cx| {
        catpaw_web::event_loop::run(cx, &catpaw_web::event_loop::LoopLimits::default());
    });
    assert_eq!(
        eval(&mut page, "log.join(' | ')"),
        "NotSupportedError | TypeError | a9993e364706816a ba7816bf8f01cfea cb00753f45a35e8b ddaf35a193617aba"
    );
}

#[test]
fn ui_events_carry_their_state() {
    let mut page = load("<button id=b>x</button><input id=i>");
    for (source, expected) in [
        (
            "var m = new MouseEvent('click', { bubbles: true, clientX: 10, clientY: 20, screenX: 30, button: 2, buttons: 2, ctrlKey: true, relatedTarget: document.body, view: window, detail: 1 }); [m instanceof UIEvent, m instanceof Event, m.type, m.bubbles, m.clientX, m.clientY, m.pageX, m.x, m.offsetY, m.screenX, m.screenY, m.button, m.buttons, m.ctrlKey, m.shiftKey, m.getModifierState('Control'), m.getModifierState('Shift'), m.relatedTarget === document.body, m.view === window, m.detail, m.isTrusted, String(m)].join(' ')",
            "true true click true 10 20 10 10 20 30 0 2 2 true false true false true true 1 false [object MouseEvent]",
        ),
        (
            "var k = new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', keyCode: 13, shiftKey: true, repeat: true }); [k instanceof UIEvent, k.key, k.code, k.keyCode, k.charCode, k.location, k.shiftKey, k.repeat, k.isComposing, k.getModifierState('Shift'), KeyboardEvent.DOM_KEY_LOCATION_NUMPAD].join(' ')",
            "true Enter Enter 13 0 0 true true false true 3",
        ),
        (
            "var p = new PointerEvent('pointerdown', { pointerId: 7, pointerType: 'touch', isPrimary: true, pressure: 0.5, clientX: 3 }); [p instanceof MouseEvent, p.pointerId, p.pointerType, p.isPrimary, p.pressure, p.width, p.clientX, p.tiltX].join(' ')",
            "true 7 touch true 0.5 1 3 0",
        ),
        (
            "var w = new WheelEvent('wheel', { deltaY: -3.5, deltaMode: 1 }); var f = new FocusEvent('blur', { relatedTarget: document.getElementById('i') }); var inp = new InputEvent('input', { data: 'a', inputType: 'insertText' }); var u = new UIEvent('resize', { detail: 2 }); [w instanceof MouseEvent, w.deltaY, w.deltaMode, WheelEvent.DOM_DELTA_LINE, f.relatedTarget.id, inp.data, inp.inputType, inp.isComposing, u.detail, u.view, u.which].map(String).join(' ')",
            "true -3.5 1 1 i a insertText false 2 null 0",
        ),
        (
            "var seen = []; var b = document.getElementById('b'); b.addEventListener('click', function (e) { seen.push(e.constructor.name, e instanceof PointerEvent, e.isTrusted, e.composed, e.bubbles, e.pointerType === ''); }); b.click(); seen.join(' ')",
            "PointerEvent true false true true true",
        ),
        (
            "var got = null; b.addEventListener('keyup', function (e) { got = e.key + e.keyCode; }); b.dispatchEvent(new KeyboardEvent('keyup', { key: 'a', keyCode: 65 })); got",
            "a65",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn processing_instructions_and_namespaced_tag_names() {
    let mut page = load(
        "<div id=d><p>a</p><svg xmlns='http://www.w3.org/2000/svg'><text>b</text><rect/></svg></div><script>function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }</script>",
    );
    for (source, expected) in [
        (
            "var pi = document.createProcessingInstruction('xml-stylesheet', 'href=\"a.css\"'); [String(pi), pi.target, pi.data, pi.nodeType, pi.nodeName, pi.nodeValue, pi.textContent, pi instanceof CharacterData, pi.ownerDocument === document].join('|')",
            "[object ProcessingInstruction]|xml-stylesheet|href=\"a.css\"|7|xml-stylesheet|href=\"a.css\"|href=\"a.css\"|true|true",
        ),
        (
            "attempt(function () { return document.createProcessingInstruction('1bad', ''); })",
            "InvalidCharacterError",
        ),
        (
            "attempt(function () { return document.createProcessingInstruction('ok', 'a?>b'); })",
            "InvalidCharacterError",
        ),
        (
            "document.body.appendChild(pi); pi.data = 'x'; new XMLSerializer().serializeToString(pi) + ' ' + document.body.lastChild.target",
            "<?xml-stylesheet x?> xml-stylesheet",
        ),
        (
            "var svg = 'http://www.w3.org/2000/svg', html = 'http://www.w3.org/1999/xhtml'; [document.getElementsByTagNameNS(svg, 'text').length, document.getElementsByTagNameNS(html, 'p').length, document.getElementsByTagNameNS('*', 'p').length, document.getElementsByTagNameNS(svg, '*').length, document.getElementsByTagNameNS(null, 'p').length, document.getElementById('d').getElementsByTagNameNS('*', '*').length, document.getElementsByTagNameNS(svg, 'TEXT').length].join(' ')",
            "1 1 1 3 0 4 0",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn named_elements_are_window_properties() {
    let mut page = load(
        "<div id=host></div><form name=f></form><img name=pic><div id=dup></div><span id=dup></span><p id=both name=both></p><script>function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }</script>",
    );
    for (source, expected) in [
        (
            "[host === document.getElementById('host'), host instanceof HTMLDivElement, window.host === host, typeof f, f.tagName, self.pic.tagName, dup instanceof HTMLCollection, dup.length, both.tagName].join(' ')",
            "true true true object FORM IMG true 2 P",
        ),
        (
            "['host' in window, 'nothing' in window, typeof nothing, attempt(function () { return nothing; }), Object.getOwnPropertyDescriptor(window, 'host') === undefined, window.hasOwnProperty('host')].join(' ')",
            "true false undefined ReferenceError true false",
        ),
        (
            "var pic = 'mine'; var alsoGlobal = 1; [pic, window.pic, typeof alsoGlobal].join(' ')",
            "mine mine number",
        ),
        (
            "document.getElementById('host').remove(); var fresh = document.createElement('b'); fresh.id = 'later'; document.body.appendChild(fresh); [typeof host, later.tagName, attempt(function () { return host; })].join(' ')",
            "undefined B ReferenceError",
        ),
        (
            "Object.getPrototypeOf(Object.getPrototypeOf(window)) !== EventTarget.prototype && Object.getPrototypeOf(Object.getPrototypeOf(Object.getPrototypeOf(window))) === EventTarget.prototype",
            "true",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn a_document_can_be_constructed() {
    let mut page = load("");
    assert_eq!(
        eval(
            &mut page,
            "var d = new Document(); [String(d), d.contentType, d.URL === document.URL, d.documentElement, d.childNodes.length, d.createElement('x').namespaceURI, d.createElement('x').localName, d instanceof Document, d instanceof XMLDocument, d.defaultView, d.compatMode].map(String).join(' ')"
        ),
        "[object Document] application/xml true null 0 null x true false null CSS1Compat"
    );
}

#[test]
fn the_selection_holds_one_range() {
    let mut page = load(
        "<p id=p>text</p><p id=q>more</p><script>function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }</script>",
    );
    for (source, expected) in [
        (
            "var s = getSelection(); var p = document.getElementById('p'), q = document.getElementById('q'); [s === document.getSelection(), Object.prototype.toString.call(s), s.type, s.rangeCount, s.isCollapsed, s.anchorNode, s.direction, s.toString() === '', attempt(function () { return s.getRangeAt(0); })].map(String).join(' ')",
            "true [object Selection] None 0 true null none true IndexSizeError",
        ),
        (
            "s.collapse(p.firstChild, 2); [s.type, s.rangeCount, s.isCollapsed, s.anchorNode === p.firstChild, s.anchorOffset, s.focusOffset, s.direction, s.getRangeAt(0) instanceof Range, s.getRangeAt(0).collapsed].join(' ')",
            "Caret 1 true true 2 2 forward true true",
        ),
        (
            "s.extend(q.firstChild, 3); [s.type, s.anchorNode === p.firstChild, s.focusNode === q.firstChild, s.direction, s.toString(), s.getRangeAt(0).startOffset, s.containsNode(p), s.containsNode(p, true), s.containsNode(q), s.containsNode(q, true)].join(' ')",
            "Range true true forward xtmor 2 false true false true",
        ),
        (
            "s.extend(p.firstChild, 0); [s.direction, s.anchorOffset, s.focusOffset, s.getRangeAt(0).startOffset, s.getRangeAt(0).endOffset, s.toString()].join(' ')",
            "backward 2 0 0 2 te",
        ),
        (
            "s.setBaseAndExtent(q.firstChild, 4, p.firstChild, 1); s.direction + ' ' + s.toString() + ' ' + s.anchorNode.data + ' ' + s.focusOffset",
            "backward extmore more 1",
        ),
        (
            "s.selectAllChildren(q); var r = s.getRangeAt(0); [r.startContainer === q, r.endOffset, s.toString(), attempt(function () { s.selectAllChildren(document.implementation.createDocumentType('html', '', '')); }), attempt(function () { s.collapse(p.firstChild, 9); })].join(' ')",
            "true 1 more InvalidNodeTypeError IndexSizeError",
        ),
        (
            "s.collapseToStart(); var caret = s.isCollapsed; s.removeAllRanges(); [caret, s.type, attempt(function () { s.collapseToEnd(); }), attempt(function () { s.extend(p, 0); })].join(' ')",
            "true None InvalidStateError InvalidStateError",
        ),
        (
            "var range = document.createRange(); range.selectNodeContents(p); s.addRange(range); var other = document.createRange(); s.addRange(other); [s.rangeCount, s.getRangeAt(0) === range, attempt(function () { s.removeRange(other); }), s.toString()].join(' ')",
            "1 true NotFoundError text",
        ),
        (
            "s.deleteFromDocument(); [p.textContent === '', s.isCollapsed, s.type].join(' '); s.removeRange(range); s.rangeCount + ' ' + p.textContent.length + ' ' + s.type",
            "0 0 None",
        ),
        (
            "String(document.implementation.createHTMLDocument('').getSelection())",
            "null",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn runaway_scripts_are_stopped() {
    let state = Rc::new(PageState::new(
        Url::parse(URL).unwrap(),
        PageConfig {
            script_budget: Some(std::time::Duration::from_millis(200)),
            ..PageConfig::default()
        },
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| {
        scripting::load_document(
            cx,
            "<script>window.before = 1; try { while (true) {} } catch (e) { window.caught = true; } window.unreached = 1;</script><script>window.after = 1</script>",
        );
    });
    assert_eq!(
        page.eval_to_string(
            "[window.before, window.caught, window.unreached, window.after].map(String).join(' ')"
        )
        .unwrap(),
        "1 undefined undefined 1"
    );
    let errors = page.page().errors.borrow().clone();
    assert!(
        errors.iter().any(|e| e.contains("time budget")),
        "{errors:?}"
    );
}
