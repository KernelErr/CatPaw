//! `MutationObserver`: which records each kind of change queues, and when
//! they are delivered.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

/// A document with a few nodes to mutate, and helpers that render the
/// records an observer receives: one log entry per callback.
const FIXTURE: &str = r#"<body><div id="root"><p id="p" class="a">text</p><span id="s"></span></div>
<script>
  var log = [];
  var root = document.getElementById('root'), p = document.getElementById('p'), s = document.getElementById('s');
  function name(n) { return n ? (n.id ? '#' + n.id : n.nodeName) : '-'; }
  function names(list) { return Array.from(list, name).join('+') || '-'; }
  function show(r) {
    if (r.type === 'childList') {
      return name(r.target) + ' +' + names(r.addedNodes) + ' -' + names(r.removedNodes) + ' ' +
        name(r.previousSibling) + '|' + name(r.nextSibling);
    }
    if (r.type === 'attributes') {
      var ns = r.attributeNamespace ? '{' + r.attributeNamespace + '}' : '';
      return name(r.target) + ' @' + ns + r.attributeName + '=' + r.oldValue;
    }
    return name(r.target) + ' "' + r.oldValue + '"';
  }
  function watch(node, options) {
    var observer = new MutationObserver(function (records) { log.push(records.map(show).join('; ')); });
    observer.observe(node, options);
    return observer;
  }
  function element(tag, id) { var e = document.createElement(tag); e.id = id; return e; }
</script>"#;

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

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

/// Runs `source` as one task and returns what observers logged for it.
fn step(page: &mut BoaPage, source: &str) -> String {
    let result = eval(page, &format!("log = []; {source}; 0"));
    assert_eq!(result, "0", "{source}");
    eval(page, "log.join(' / ')")
}

#[test]
fn records_arrive_in_order_in_one_batch() {
    let mut page = load(FIXTURE);
    let delivered = step(
        &mut page,
        "var observer = new MutationObserver(function (records, self) {
           log.push(records.length + ' ' + (self === observer) + ' ' + (this === observer) + ' ' +
             (records[0] instanceof MutationRecord) + ' ' + (records[0].addedNodes === records[0].addedNodes));
           log.push(records.map(show).join('; '));
         });
         observer.observe(root, { childList: true, subtree: true, attributes: true, attributeOldValue: true,
           characterData: true, characterDataOldValue: true });
         root.appendChild(element('b', 'n'));
         p.className = 'b';
         p.firstChild.data = 'changed';
         root.removeChild(p);
         log.push('end of script')",
    );
    assert_eq!(
        delivered,
        "end of script / 4 true true true true / \
         #root +#n -- #s|-; #p @class=a; #text \"text\"; #root +- -#p -|#s"
    );
}

#[test]
fn observers_are_notified_from_a_microtask() {
    let mut page = load(FIXTURE);
    assert_eq!(
        eval(
            &mut page,
            "var order = [];
             new MutationObserver(function () { order.push('observer'); }).observe(root, { attributes: true });
             Promise.resolve().then(function () { order.push('promise 1'); });
             root.setAttribute('a', '1');
             root.setAttribute('a', '2');
             Promise.resolve().then(function () { order.push('promise 2'); });
             queueMicrotask(function () { order.push('microtask'); });
             order.push('script');
             0"
        ),
        "0"
    );
    assert_eq!(
        eval(&mut page, "order.join()"),
        "script,promise 1,observer,promise 2,microtask"
    );

    // What a callback changes is reported in a later microtask of the same
    // checkpoint, to every interested observer.
    assert_eq!(
        step(
            &mut page,
            "var rounds = 0;
             watch(s, { attributes: true });
             new MutationObserver(function () { if (++rounds < 3) s.setAttribute('round', rounds); })
               .observe(s, { attributes: true });
             s.setAttribute('round', 0)"
        ),
        "#s @round=null / #s @round=null / #s @round=null"
    );
    assert_eq!(eval(&mut page, "rounds"), "3");
}

#[test]
fn tree_changes_queue_one_record_per_operation() {
    let mut page = load(FIXTURE);
    eval(&mut page, "watch(root, { childList: true }); 0");
    // root: p s
    assert_eq!(
        step(
            &mut page,
            "var n = element('b', 'n'); root.replaceChild(n, p)"
        ),
        "#root +#n -#p -|#s"
    );
    // root: n s. A fragment's children arrive together.
    assert_eq!(
        step(
            &mut page,
            "var f = document.createDocumentFragment(), a = element('i', 'a'), b = element('i', 'b');
             f.append(a, b);
             root.insertBefore(f, s)"
        ),
        "#root +#a+#b -- #n|#s"
    );
    // root: n a b s. A node that moves is removed, then added.
    assert_eq!(
        step(&mut page, "root.appendChild(n)"),
        "#root +- -#n -|#a; #root +#n -- #s|-"
    );
    // root: a b s n
    assert_eq!(step(&mut page, "s.remove()"), "#root +- -#s #b|#n");
    // root: a b n
    assert_eq!(
        step(&mut page, "b.after('tail', a)"),
        "#root +- -#a -|#b; #root +#text+#a -- #b|#n"
    );
    // root: b "tail" a n
    assert_eq!(
        step(&mut page, "root.replaceChildren(p)"),
        "#root +#p -#b+#text+#a+#n -|-"
    );
    assert_eq!(
        step(&mut page, "root.textContent = 'x'"),
        "#root +#text -#p -|-"
    );
    assert_eq!(
        step(
            &mut page,
            "root.innerHTML = '<u id=\"u1\"></u><u id=\"u2\"></u>'"
        ),
        "#root +#u1+#u2 -#text -|-"
    );
    assert_eq!(
        step(
            &mut page,
            "root.insertAdjacentHTML('afterbegin', '<u id=\"u0\"></u>')"
        ),
        "#root +#u0 -- -|#u1"
    );
    assert_eq!(
        step(&mut page, "root.textContent = ''"),
        "#root +- -#u0+#u1+#u2 -|-"
    );
    // Nothing to remove and nothing to add: no record.
    assert_eq!(step(&mut page, "root.textContent = ''"), "");
    // A fragment that gives up its children has a record of its own.
    assert_eq!(
        step(
            &mut page,
            "var g = document.createDocumentFragment();
             g.append(element('i', 'g1'), element('i', 'g2'));
             watch(g, { childList: true });
             root.appendChild(g)"
        ),
        "#document-fragment +- -#g1+#g2 -|- / #root +#g1+#g2 -- -|-"
    );
}

#[test]
fn text_nodes_split_and_normalize() {
    let mut page = load(FIXTURE);
    eval(
        &mut page,
        "watch(root, { childList: true, characterData: true, characterDataOldValue: true, subtree: true }); 0",
    );
    assert_eq!(
        step(&mut page, "var t = p.firstChild; t.splitText(2)"),
        "#p +#text -- #text|-; #text \"text\""
    );
    assert_eq!(
        step(&mut page, "p.appendChild(document.createTextNode(''))"),
        "#p +#text -- #text|-"
    );
    assert_eq!(
        step(&mut page, "root.normalize()"),
        "#text \"te\"; #p +- -#text #text|#text; #p +- -#text #text|-"
    );
    assert_eq!(
        eval(
            &mut page,
            "p.childNodes.length + ' ' + (p.firstChild === t) + ' ' + t.data"
        ),
        "1 true text"
    );
    // Nothing left to merge.
    assert_eq!(step(&mut page, "root.normalize()"), "");
}

#[test]
fn subtrees_are_followed_until_the_next_notification() {
    let mut page = load(FIXTURE);
    eval(
        &mut page,
        "watch(root, { childList: true, attributes: true, subtree: true }); 0",
    );
    // What happens to a tree before it is attached is not reported.
    assert_eq!(
        step(
            &mut page,
            "var d = element('div', 'd');
             d.appendChild(document.createElement('i'));
             root.appendChild(d)"
        ),
        "#root +#d -- #s|-"
    );
    // A removed subtree is still observed for the rest of the task...
    assert_eq!(
        step(
            &mut page,
            "root.removeChild(p);
             p.setAttribute('x', '1');
             p.appendChild(document.createElement('b'))"
        ),
        "#root +- -#p -|#s; #p @x=null; #p +B -- #text|-"
    );
    // ...and no longer.
    assert_eq!(step(&mut page, "p.setAttribute('y', '2')"), "");
    assert_eq!(step(&mut page, "s.setAttribute('y', '2')"), "#s @y=null");

    // Without `subtree` only the node itself is observed.
    assert_eq!(
        step(
            &mut page,
            "var own = []; var o = new MutationObserver(function (records) { own.push(records.map(show).join('; ')); });
             o.observe(d, { attributes: true, childList: true });
             d.firstChild.setAttribute('k', 'v');
             d.firstChild.appendChild(document.createElement('b'));
             d.setAttribute('k', 'v')"
        ),
        "I @k=null; I +B -- -|-; #d @k=null"
    );
    assert_eq!(eval(&mut page, "own.join(' / ')"), "#d @k=null");
}

#[test]
fn attribute_changes_are_reported_whatever_makes_them() {
    let mut page = load(FIXTURE);
    eval(
        &mut page,
        "watch(p, { attributes: true, attributeOldValue: true }); 0",
    );
    assert_eq!(step(&mut page, "p.style.color = 'red'"), "#p @style=null");
    assert_eq!(step(&mut page, "p.classList.add('k')"), "#p @class=a");
    assert_eq!(step(&mut page, "p.dataset.k = 'v'"), "#p @data-k=null");
    assert_eq!(
        step(
            &mut page,
            "p.toggleAttribute('hidden'); p.removeAttribute('hidden')"
        ),
        "#p @hidden=null; #p @hidden="
    );
    // Setting the value an attribute already has is still a change.
    assert_eq!(
        step(&mut page, "p.setAttribute('class', 'a k')"),
        "#p @class=a k"
    );
    assert_eq!(
        step(
            &mut page,
            "p.setAttributeNS('http://www.w3.org/1999/xlink', 'xlink:href', '#a')"
        ),
        "#p @{http://www.w3.org/1999/xlink}href=null"
    );
    assert_eq!(step(&mut page, "p.id = 'q'"), "#q @id=p");
    // Removing an attribute that is not there changes nothing.
    assert_eq!(step(&mut page, "p.removeAttribute('nope')"), "");

    // A filter lists attributes without a namespace; old values are only
    // kept when asked for.
    assert_eq!(
        step(
            &mut page,
            "watch(s, { attributeFilter: ['data-a', 'href'] });
             s.setAttribute('data-b', '1');
             s.setAttribute('data-a', '1');
             s.setAttributeNS('http://www.w3.org/1999/xlink', 'xlink:href', '#a');
             s.removeAttribute('data-a')"
        ),
        "#s @data-a=null; #s @data-a=null"
    );
}

#[test]
fn records_can_be_taken_and_observers_disconnected() {
    let mut page = load(FIXTURE);
    eval(
        &mut page,
        "var observer = watch(root, { childList: true }); 0",
    );
    // Taken records are not delivered.
    assert_eq!(
        step(
            &mut page,
            "root.appendChild(element('i', 'x'));
             var taken = observer.takeRecords();
             log.push('took ' + taken.map(show).join('; ') + ', left ' + observer.takeRecords().length)"
        ),
        "took #root +#x -- #s|-, left 0"
    );
    // Observing a node again replaces the options; one observer can watch
    // several nodes.
    assert_eq!(
        step(
            &mut page,
            "observer.observe(root, { attributes: true });
             observer.observe(p, { characterData: true, subtree: true });
             root.appendChild(element('i', 'y'));
             root.setAttribute('k', 'v');
             p.firstChild.data = 'new'"
        ),
        "#root @k=null; #text \"null\""
    );
    // Disconnecting drops the registrations and the queue.
    assert_eq!(
        step(
            &mut page,
            "root.setAttribute('k', 'w');
             observer.disconnect();
             p.firstChild.data = 'newer';
             root.setAttribute('k', 'x');
             log.push('left ' + observer.takeRecords().length)"
        ),
        "left 0"
    );
    // A disconnected observer can be used again.
    assert_eq!(
        step(
            &mut page,
            "observer.observe(root, { attributes: true }); root.setAttribute('k', 'y')"
        ),
        "#root @k=null"
    );
}

#[test]
fn options_are_validated() {
    let mut page = load(FIXTURE);
    eval(
        &mut page,
        "function attempt(options) {
           try { new MutationObserver(function () {}).observe(root, options); return 'ok'; }
           catch (e) { return e.name; }
         }
         0",
    );
    for (options, outcome) in [
        ("{}", "TypeError"),
        ("{ subtree: true }", "TypeError"),
        ("{ childList: true }", "ok"),
        ("{ attributeOldValue: true }", "ok"),
        ("{ attributeFilter: [] }", "ok"),
        ("{ characterDataOldValue: true }", "ok"),
        (
            "{ attributes: false, attributeOldValue: true }",
            "TypeError",
        ),
        ("{ attributes: false, attributeFilter: ['a'] }", "TypeError"),
        (
            "{ characterData: false, characterDataOldValue: true }",
            "TypeError",
        ),
        ("{ childList: true, attributes: false }", "ok"),
    ] {
        assert_eq!(
            eval(&mut page, &format!("attempt({options})")),
            outcome,
            "{options}"
        );
    }
    assert_eq!(
        eval(
            &mut page,
            "try { new MutationObserver(function () {}).observe(null, { childList: true }); 'ok' } catch (e) { e.name }"
        ),
        "TypeError"
    );
    assert_eq!(
        eval(
            &mut page,
            "try { new MutationObserver(); 'ok' } catch (e) { e.name }"
        ),
        "TypeError"
    );
}

#[test]
fn the_parser_is_observed() {
    let mut page = load(
        r#"<script>
  var seen = [];
  new MutationObserver(function (records) {
    seen.push(records.map(function (r) {
      return Array.from(r.addedNodes, function (n) { return n.nodeName; }).join('+') + '>' + r.target.nodeName;
    }).join(' '));
  }).observe(document, { childList: true, subtree: true });
</script><body><main><h1>title</h1></main><script>document.write('<em>w</em>');</script><p>after</p>"#,
    );
    assert_eq!(
        eval(&mut page, "seen.join(' / ')"),
        "BODY>HTML MAIN>BODY H1>MAIN #text>H1 SCRIPT>BODY #text>SCRIPT / EM>BODY #text>EM / P>BODY #text>P"
    );
}

#[test]
fn observers_outlive_their_script_references_and_survive_errors() {
    let mut page = load(FIXTURE);
    eval(
        &mut page,
        "var fired = 0;
         (function () {
           new MutationObserver(function () { fired++; throw new Error('boom'); }).observe(root, { attributes: true });
           new MutationObserver(function () { fired += 10; }).observe(root, { attributes: true });
         })();
         0",
    );
    page.with_cx(|cx| cx.script.collect_garbage());
    assert_eq!(eval(&mut page, "root.setAttribute('k', 'v'); 0"), "0");
    assert_eq!(eval(&mut page, "fired"), "11");
    let errors = page.page().errors.borrow().clone();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("boom"), "{errors:?}");
}
