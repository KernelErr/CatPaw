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
            "typeof WebSocket + ' ' + ('serviceWorker' in navigator)"
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
