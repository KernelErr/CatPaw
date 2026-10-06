//! User timing and the performance timeline: marks, measures and the
//! observers that hear of them.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><body>
<script>
  var log = [];
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  function names(entries) { return entries.map(function (e) { return e.entryType + ':' + e.name; }).join('+'); }
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

fn check(page: &mut BoaPage, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(eval(page, source), *expected, "{source}");
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
fn marks_and_measures_are_recorded() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "var now = performance.now(), m = performance.mark('first');
                 [m instanceof PerformanceMark, m instanceof PerformanceEntry, m.name, m.entryType, m.duration, m.startTime >= now, m.detail].join()",
                "true,true,first,mark,0,true,",
            ),
            ("performance.getEntriesByName('first')[0] === m && performance.getEntries()[0] === m", "true"),
            ("performance.clearMarks('first'); performance.getEntries().length", "0"),
            // Entries come back in the order they started.
            (
                "performance.mark('a', { startTime: 10 }); performance.mark('b', { startTime: 5 }); performance.mark('c', { startTime: 7 });
                 names(performance.getEntries())",
                "mark:b+mark:c+mark:a",
            ),
            (
                "var d = { x: [1] }, kept = performance.mark('c', { startTime: 8, detail: d });
                 kept.detail !== d && kept.detail === kept.detail && kept.detail.x[0]",
                "1",
            ),
            (
                "var ab = performance.measure('ab', 'b', 'a'); [ab instanceof PerformanceMeasure, ab.entryType, ab.startTime, ab.duration, ab.detail].join()",
                "true,measure,5,5,",
            ),
            // The latest mark of a name is the one that counts.
            ("performance.measure('to c', 'b', 'c').duration", "3"),
            ("var o = performance.measure('opts', { start: 2, end: 9, detail: 'd' }); [o.startTime, o.duration, o.detail].join()", "2,7,d"),
            ("var s = performance.measure('start+duration', { start: 'b', duration: 3 }); s.startTime + ' ' + s.duration", "5 3"),
            ("var e = performance.measure('end-duration', { end: 10, duration: 4 }); e.startTime + ' ' + e.duration", "6 4"),
            ("var n = performance.measure('from navigation', 'navigationStart', 'a'); n.startTime + ' ' + n.duration", "0 10"),
            ("var w = performance.measure('so far'); w.startTime === 0 && w.duration >= now", "true"),
            (
                "[performance.getEntriesByType('mark').length, performance.getEntriesByType('measure').length, performance.getEntriesByType('resource').length,
                  performance.getEntriesByName('ab', 'measure').length, performance.getEntriesByName('ab', 'mark').length, performance.getEntriesByName('c').length].join()",
                "4,7,0,1,0,2",
            ),
            ("Object.keys(ab.toJSON()).join() + ' ' + JSON.stringify(o)", "name,entryType,startTime,duration,detail {\"name\":\"opts\",\"entryType\":\"measure\",\"startTime\":2,\"duration\":7,\"detail\":\"d\"}"),
            // A mark made by hand is not on the timeline.
            ("new PerformanceMark('free', { startTime: 1 }).startTime + ' ' + performance.getEntriesByName('free').length", "1 0"),
            ("performance.clearMarks('c'); names(performance.getEntriesByType('mark'))", "mark:b+mark:a"),
            ("performance.clearMarks(); performance.clearMeasures('ab'); performance.getEntries().length", "6"),
            ("performance.clearMeasures(); performance.getEntries().length", "0"),
        ],
    );
}

#[test]
fn what_cannot_be_timed_is_refused() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "performance.mark('a', { startTime: 1 }); attempt(function () { performance.measure('x', 'nope'); })",
                "SyntaxError",
            ),
            (
                "attempt(function () { performance.measure('x', 'a', 'nope'); })",
                "SyntaxError",
            ),
            (
                "attempt(function () { performance.measure('x', { start: 1, end: 2, duration: 3 }); })",
                "TypeError",
            ),
            (
                "attempt(function () { performance.measure('x', { duration: 3 }); })",
                "TypeError",
            ),
            (
                "attempt(function () { performance.measure('x', { start: 1 }, 'a'); })",
                "TypeError",
            ),
            (
                "attempt(function () { performance.measure('x', { start: -1 }); })",
                "TypeError",
            ),
            (
                "attempt(function () { performance.mark('navigationStart'); })",
                "SyntaxError",
            ),
            (
                "attempt(function () { performance.mark('x', { startTime: -1 }); })",
                "TypeError",
            ),
            (
                "attempt(function () { performance.mark('x', { startTime: NaN }); })",
                "TypeError",
            ),
            // The navigation's own moments are not timed.
            (
                "attempt(function () { performance.measure('x', 'domComplete'); })",
                "InvalidAccessError",
            ),
            ("attempt(function () { performance.mark(); })", "TypeError"),
            ("performance.getEntries().length", "1"),
        ],
    );
}

#[test]
fn observers_hear_of_new_entries() {
    let mut page = load(FIXTURE);
    assert_eq!(
        eval(&mut page, "PerformanceObserver.supportedEntryTypes.join()"),
        "mark,measure"
    );
    assert_eq!(
        step(
            &mut page,
            "var po = new PerformanceObserver(function (list, observer, options) {
               log.push(names(list.getEntries()) + ' ' + (observer === po) + ' ' + (this === po) + ' ' + JSON.stringify(options) + ' ' +
                 (list instanceof PerformanceObserverEntryList) + ' ' + list.getEntriesByType('mark').length + ' ' + list.getEntriesByName('me').length);
             });
             po.observe({ entryTypes: ['mark', 'measure', 'paint'] });
             performance.mark('m1', { startTime: 1 });
             performance.measure('me', { start: 0, end: 4 });
             performance.mark('m2', { startTime: 2 });
             log.push('script')"
        ),
        "script / measure:me+mark:m1+mark:m2 true true {\"droppedEntriesCount\":0} true 2 1"
    );
    assert_eq!(
        step(&mut page, "performance.mark('m3')"),
        "mark:m3 true true {} true 1 0"
    );
    // Records taken by hand are not delivered.
    assert_eq!(
        step(
            &mut page,
            "performance.mark('m4'); log.push('took ' + names(po.takeRecords()))"
        ),
        "took mark:m4"
    );
    assert_eq!(
        step(&mut page, "po.disconnect(); performance.mark('m5')"),
        ""
    );
}

#[test]
fn observers_can_ask_for_what_is_already_there() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "performance.mark('early', { startTime: 1 }); performance.measure('early measure', { start: 0, end: 1 });
             var single = new PerformanceObserver(function (list) { log.push(names(list.getEntries())); });
             single.observe({ type: 'mark', buffered: true });
             performance.mark('late', { startTime: 2 })"
        ),
        "mark:early+mark:late"
    );
    // Types can be added one at a time.
    assert_eq!(
        step(
            &mut page,
            "single.observe({ type: 'measure' }); performance.measure('m', { start: 0, end: 1 }); performance.mark('again', { startTime: 3 })"
        ),
        "measure:m+mark:again"
    );
    check(
        &mut page,
        &[
            ("attempt(function () { single.observe(); })", "TypeError"),
            (
                "attempt(function () { single.observe({ entryTypes: ['mark'], type: 'mark' }); })",
                "TypeError",
            ),
            (
                "attempt(function () { single.observe({ entryTypes: ['mark'], buffered: true }); })",
                "TypeError",
            ),
            (
                "attempt(function () { single.observe({ entryTypes: ['mark'] }); })",
                "InvalidModificationError",
            ),
            // Types nothing is recorded for are passed over.
            (
                "attempt(function () { new PerformanceObserver(function () {}).observe({ type: 'largest-contentful-paint', buffered: true }); return 'ok'; })",
                "ok",
            ),
            (
                "attempt(function () { new PerformanceObserver(function () {}).observe({ entryTypes: ['paint', 'resource'] }); return 'ok'; })",
                "ok",
            ),
            (
                "attempt(function () { return new PerformanceObserver(); })",
                "TypeError",
            ),
        ],
    );
}
