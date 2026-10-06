//! `IntersectionObserver` and `ResizeObserver` without layout: boxes are
//! empty, so targets intersect their root when they are in its tree, and
//! no element has a size to report.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<body><div id="a"><i id="inner"></i></div><div id="b"></div>
<script>
  var log = [];
  var a = document.getElementById('a'), b = document.getElementById('b'), inner = document.getElementById('inner');
  function show(entries) {
    return entries.map(function (e) { return e.target.id + ' ' + e.isIntersecting + ' ' + e.intersectionRatio; }).join(', ');
  }
  function watch(options) {
    return new IntersectionObserver(function (entries) { log.push(show(entries)); }, options);
  }
  function rect(r) { return [r.x, r.y, r.width, r.height].join(' '); }
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
</script>"#;

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

/// Runs `source`, lets the page settle, and returns what was logged.
fn step(page: &mut BoaPage, source: &str) -> String {
    let result = eval(page, &format!("log = []; {source}; 0"));
    assert_eq!(result, "0", "{source}");
    settle(page);
    eval(page, "log.join(' / ')")
}

#[test]
fn targets_are_reported_once_and_when_they_enter_or_leave_the_tree() {
    let mut page = load(FIXTURE);
    // The first look happens in the next frame, after its callbacks.
    assert_eq!(
        step(
            &mut page,
            "var io = new IntersectionObserver(function (entries, self) {
               log.push(show(entries) + ' | ' + (self === io) + ' ' + (this === io) + ' ' +
                 (entries[0] instanceof IntersectionObserverEntry));
             });
             var d = document.createElement('div'); d.id = 'd';
             io.observe(a); io.observe(d); io.observe(b); io.observe(a);
             requestAnimationFrame(function () { log.push('frame ' + io.takeRecords().length); });
             Promise.resolve().then(function () { log.push('microtask'); });
             log.push('script')"
        ),
        "script / microtask / frame 0 / a true 1, d false 0, b true 1 | true true true"
    );
    // Nothing changed: nothing to report, and the page comes to rest.
    assert_eq!(step(&mut page, "a.className = 'x'"), "");
    assert_eq!(
        step(&mut page, "document.body.appendChild(d); a.remove()"),
        "a false 0, d true 1 | true true true"
    );
    assert_eq!(
        step(&mut page, "io.unobserve(d); d.remove(); b.remove()"),
        "b false 0 | true true true"
    );
    assert_eq!(
        step(&mut page, "io.disconnect(); document.body.append(a, b)"),
        ""
    );
    // Observing again starts over.
    assert_eq!(
        step(&mut page, "io.observe(a)"),
        "a true 1 | true true true"
    );
}

#[test]
fn entries_describe_empty_boxes() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "new IntersectionObserver(function (entries) {
               var e = entries[0];
               log.push([typeof e.time, e.time > 0, rect(e.rootBounds), rect(e.boundingClientRect),
                 rect(e.intersectionRect), e.rootBounds instanceof DOMRectReadOnly].join(' | '));
             }, { rootMargin: '10px 5%' }).observe(a)"
        ),
        "number | true | -64 -10 1408 740 | 0 0 0 0 | 0 0 0 0 | true"
    );
    // An element root has an empty box of its own.
    assert_eq!(
        step(
            &mut page,
            "new IntersectionObserver(function (entries) {
               log.push(entries.map(function (e) { return e.target.id + ' ' + e.isIntersecting + ' ' + rect(e.rootBounds); }).join(', '));
             }, { root: a, rootMargin: '1px 2px 3px 4px' }).observe(inner)"
        ),
        "inner true -4 -1 6 4"
    );
}

#[test]
fn roots_limit_what_intersects() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var within = watch({ root: a }); within.observe(inner); within.observe(b)"
        ),
        "inner true 1, b false 0"
    );
    assert_eq!(step(&mut page, "b.appendChild(inner)"), "inner false 0");
    assert_eq!(
        step(&mut page, "a.appendChild(b)"),
        "inner true 1, b true 1"
    );
    // An observer whose root is out of the document is passed over.
    assert_eq!(step(&mut page, "a.remove(); inner.remove()"), "");
    assert_eq!(
        step(&mut page, "document.body.appendChild(a)"),
        "inner false 0"
    );
    assert_eq!(
        step(
            &mut page,
            "var doc = watch({ root: document }); doc.observe(a); doc.observe(document.createElement('p'))"
        ),
        "a true 1,  false 0"
    );
}

#[test]
fn options_are_parsed_and_validated() {
    let mut page = load(FIXTURE);
    for (source, expected) in [
        ("watch().rootMargin", "0px 0px 0px 0px"),
        (
            "watch({ rootMargin: '10px' }).rootMargin",
            "10px 10px 10px 10px",
        ),
        (
            "watch({ rootMargin: ' 10px  -5.5% ' }).rootMargin",
            "10px -5.5% 10px -5.5%",
        ),
        (
            "watch({ rootMargin: '1px 2px 3px' }).rootMargin",
            "1px 2px 3px 2px",
        ),
        (
            "watch({ rootMargin: '1PX 2px 3px 4%' }).rootMargin",
            "1px 2px 3px 4%",
        ),
        ("watch({ rootMargin: '' }).rootMargin", "0px 0px 0px 0px"),
        (
            "watch({ scrollMargin: '2px' }).scrollMargin",
            "2px 2px 2px 2px",
        ),
        (
            "attempt(function () { return watch({ rootMargin: '1em' }); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { return watch({ rootMargin: 'auto' }); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { return watch({ rootMargin: '0' }); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { return watch({ rootMargin: '1px 2px 3px 4px 5px' }); })",
            "SyntaxError",
        ),
        ("watch().thresholds.join()", "0"),
        ("watch({ threshold: 0.5 }).thresholds.join()", "0.5"),
        (
            "watch({ threshold: [1, 0.25, 0] }).thresholds.join()",
            "0,0.25,1",
        ),
        ("watch({ threshold: [] }).thresholds.join()", "0"),
        (
            "attempt(function () { return watch({ threshold: 1.5 }); })",
            "RangeError",
        ),
        (
            "attempt(function () { return watch({ threshold: [0, -1] }); })",
            "RangeError",
        ),
        (
            "attempt(function () { return watch({ threshold: NaN }); })",
            "TypeError",
        ),
        ("watch().root", "null"),
        ("watch({ root: a }).root === a", "true"),
        ("watch({ root: document }).root === document", "true"),
        (
            "attempt(function () { return watch({ root: document.createTextNode('') }); })",
            "TypeError",
        ),
        (
            "attempt(function () { return new IntersectionObserver(); })",
            "TypeError",
        ),
        (
            "attempt(function () { watch().observe(document); })",
            "TypeError",
        ),
        (
            "attempt(function () { watch().observe(null); })",
            "TypeError",
        ),
        (
            "'isVisible' in IntersectionObserverEntry.prototype",
            "false",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn records_can_be_taken_before_they_are_delivered() {
    let mut page = load(FIXTURE);
    // Observers are notified in turn; the first takes the second's entries.
    assert_eq!(
        step(
            &mut page,
            "var second;
             var first = new IntersectionObserver(function (entries) {
               log.push('first ' + show(entries) + ' / took ' + show(second.takeRecords()) + ' / left ' + second.takeRecords().length);
             });
             second = new IntersectionObserver(function () { log.push('second'); });
             first.observe(a); second.observe(b)"
        ),
        "first a true 1 / took b true 1 / left 0"
    );
    // Both still work afterwards.
    assert_eq!(
        step(&mut page, "a.remove(); b.remove()"),
        "first a false 0 / took b false 0 / left 0"
    );
}

#[test]
fn observers_outlive_their_script_references_and_survive_errors() {
    let mut page = load(FIXTURE);
    eval(
        &mut page,
        "var fired = 0;
         (function () {
           new IntersectionObserver(function () { fired++; throw new Error('boom'); }).observe(a);
           new IntersectionObserver(function () { fired += 10; }).observe(a);
         })();
         0",
    );
    page.with_cx(|cx| cx.script.collect_garbage());
    settle(&mut page);
    assert_eq!(eval(&mut page, "fired"), "11");
    let errors = page.page().errors.borrow().clone();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("boom"), "{errors:?}");
}

#[test]
fn resize_observers_have_nothing_to_report() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var ro = new ResizeObserver(function () { log.push('resized'); });
             ro.observe(a);
             ro.observe(a, { box: 'border-box' });
             ro.observe(b);
             a.style.width = '100px';
             ro.unobserve(b);
             log.push(typeof ro.disconnect)"
        ),
        "function"
    );
    for (source, expected) in [
        (
            "attempt(function () { ro.disconnect(); return 'ok'; })",
            "ok",
        ),
        (
            "attempt(function () { return new ResizeObserver(); })",
            "TypeError",
        ),
        (
            "attempt(function () { ro.observe(document); })",
            "TypeError",
        ),
        (
            "attempt(function () { ro.observe(a, { box: 'margin-box' }); })",
            "TypeError",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}
