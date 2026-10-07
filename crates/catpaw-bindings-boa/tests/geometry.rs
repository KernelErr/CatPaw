//! The CSSOM View from script: rectangles, offsets, scrolling and hit
//! testing over real layout.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig {
            time_origin_unix_ms: Some(1_700_000_000_000.0),
            viewport_width: 800,
            viewport_height: 600,
            ..PageConfig::default()
        },
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

fn settle(page: &mut BoaPage) {
    page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
}

const RECT: &str = "function rect(id) { var r = document.getElementById(id).getBoundingClientRect(); return [r.x, r.y, r.width, r.height].map(Math.round).join(','); }";

#[test]
fn bounding_rects_come_from_layout() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><div id=a style="height:50px"></div><div id=b style="margin-left:20px;width:100px;height:30px"></div>"#,
    );
    eval(&mut page, RECT);
    assert_eq!(eval(&mut page, "rect('a')"), "0,0,800,50");
    assert_eq!(eval(&mut page, "rect('b')"), "20,50,100,30");
    assert_eq!(
        eval(
            &mut page,
            "var r = document.getElementById('b').getBoundingClientRect(); [r.top, r.right, r.bottom, r.left].join(',')"
        ),
        "50,120,80,20"
    );
    assert_eq!(
        eval(
            &mut page,
            "JSON.stringify(document.getElementById('a').getBoundingClientRect())"
        ),
        r#"{"x":0,"y":0,"width":800,"height":50,"top":0,"right":800,"bottom":50,"left":0}"#
    );
}

#[test]
fn layout_follows_the_document() {
    let mut page =
        load(r#"<!doctype html><body style="margin:0"><div id=a style="height:50px"></div>"#);
    eval(&mut page, RECT);
    assert_eq!(eval(&mut page, "rect('a')"), "0,0,800,50");
    eval(
        &mut page,
        "document.getElementById('a').style.height = '70px'",
    );
    assert_eq!(eval(&mut page, "rect('a')"), "0,0,800,70");
    eval(
        &mut page,
        "var s = document.createElement('style'); s.textContent = '#a { width: 300px }'; document.head.appendChild(s)",
    );
    assert_eq!(eval(&mut page, "rect('a')"), "0,0,300,70");
    // A paragraph brings its 1em margins; the top one collapses through the body.
    eval(
        &mut page,
        "document.body.insertBefore(document.createElement('p'), document.getElementById('a')).style.height = '10px'",
    );
    assert_eq!(eval(&mut page, "rect('a')"), "0,42,300,70");
}

#[test]
fn inline_elements_have_client_rects_per_line() {
    let mut page = load(
        r#"<!doctype html><div style="width:100px;font:16px sans-serif"><span id=s>word word word word word word word word</span></div>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "var l = document.getElementById('s').getClientRects(); l.length > 1 && l[0].top < l[1].top && l instanceof DOMRectList && l.item(0).width > 0"
        ),
        "true"
    );
    assert_eq!(
        eval(
            &mut page,
            "var r = document.getElementById('s').getBoundingClientRect(); r.height > l[0].height"
        ),
        "true"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.createElement('div').getBoundingClientRect().width"
        ),
        "0"
    );
}

#[test]
fn offsets_and_client_sizes() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><div id=outer style="position:relative;margin:10px;padding:5px;border:2px solid"><div id=inner style="height:40px;padding:3px;border:1px solid"></div></div>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "var i = document.getElementById('inner'); [i.offsetParent.id, i.offsetLeft, i.offsetTop, i.offsetWidth, i.offsetHeight].join(',')"
        ),
        "outer,5,5,766,48"
    );
    assert_eq!(
        eval(
            &mut page,
            "[i.clientLeft, i.clientTop, i.clientWidth, i.clientHeight].join(',')"
        ),
        "1,1,764,46"
    );
    assert_eq!(
        eval(
            &mut page,
            "var o = document.getElementById('outer'); [o.offsetParent === document.body, o.offsetLeft, o.offsetTop].join(',')"
        ),
        "true,10,10"
    );
    assert_eq!(
        eval(
            &mut page,
            "[document.documentElement.clientWidth, document.documentElement.clientHeight, innerWidth, innerHeight].join(',')"
        ),
        "800,600,800,600"
    );
    assert_eq!(eval(&mut page, "document.body.offsetParent"), "null");
}

#[test]
fn elements_scroll_within_their_content() {
    let mut page = load(
        r#"<!doctype html><div id=sc style="overflow:auto;width:100px;height:50px"><div id=tall style="height:300px;width:150px"></div></div>
        <script>var events = []; document.getElementById('sc').addEventListener('scroll', function (e) { events.push(e.target.id + ':' + e.bubbles); });</script>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "var sc = document.getElementById('sc'); [sc.scrollWidth, sc.scrollHeight, sc.clientWidth, sc.clientHeight, sc.scrollTop].join(',')"
        ),
        "150,300,100,50,0"
    );
    eval(&mut page, "sc.scrollTop = 120");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "sc.scrollTop + ' ' + events.join(' ')"),
        "120 sc:false"
    );
    assert_eq!(
        eval(
            &mut page,
            "Math.round(document.getElementById('tall').getBoundingClientRect().top - sc.getBoundingClientRect().top)"
        ),
        "-120"
    );
    eval(&mut page, "sc.scrollTo({top: 9999, left: 40})");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "sc.scrollTop + ',' + sc.scrollLeft"),
        "250,40"
    );
    eval(&mut page, "sc.scrollBy(-10, -10)");
    assert_eq!(
        eval(&mut page, "sc.scrollTop + ',' + sc.scrollLeft"),
        "240,30"
    );
    // A block that does not scroll stays put.
    eval(&mut page, "document.body.scrollTop = 50");
    assert_eq!(eval(&mut page, "document.body.scrollTop"), "0");
}

#[test]
fn the_window_scrolls_the_document() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><div style="height:2000px"></div><div id=end style="height:10px"></div>
        <script>var fired = 0; addEventListener('scroll', function () { fired++; });</script>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "[document.documentElement.scrollHeight, document.scrollingElement === document.documentElement, scrollY].join(',')"
        ),
        "2010,true,0"
    );
    eval(&mut page, "scrollTo(0, 500)");
    settle(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "[scrollY, pageYOffset, document.documentElement.scrollTop, fired, Math.round(document.getElementById('end').getBoundingClientRect().top)].join(',')"
        ),
        "500,500,500,1,1500"
    );
    eval(&mut page, "scrollBy({top: 100000})");
    settle(&mut page);
    assert_eq!(eval(&mut page, "scrollY"), "1410");
    eval(
        &mut page,
        "document.documentElement.scrollTop = 0; document.getElementById('end').scrollIntoView()",
    );
    settle(&mut page);
    assert_eq!(eval(&mut page, "scrollY"), "1410");
    eval(
        &mut page,
        "scrollTo(0, 0); document.getElementById('end').scrollIntoView({block: 'end'})",
    );
    settle(&mut page);
    assert_eq!(eval(&mut page, "scrollY"), "1410");
}

#[test]
fn element_from_point_sees_the_layout() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><div id=a style="height:100px"><button id=b style="position:absolute;left:300px;top:20px;width:80px;height:30px">go</button></div><div id=c style="height:100px"></div><div style="height:2000px"></div>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "[document.elementFromPoint(10, 10).id, document.elementFromPoint(320, 30).id, document.elementFromPoint(10, 150).id, document.elementFromPoint(10, 400).tagName, String(document.elementFromPoint(-1, 10))].join(',')"
        ),
        "a,b,c,DIV,null"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.elementsFromPoint(320, 30).map(function (e) { return e.id || e.tagName; }).join(',')"
        ),
        "b,a,BODY,HTML"
    );
    eval(&mut page, "scrollTo(0, 120)");
    assert_eq!(eval(&mut page, "document.elementFromPoint(10, 10).id"), "c");
}

#[test]
fn fixed_boxes_stay_put_when_the_window_scrolls() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><div style="height:3000px"></div><div id=f style="position:fixed;bottom:0;right:0;width:50px;height:20px"></div>"#,
    );
    eval(&mut page, RECT);
    assert_eq!(eval(&mut page, "rect('f')"), "750,580,50,20");
    eval(&mut page, "scrollTo(0, 1000)");
    assert_eq!(eval(&mut page, "rect('f')"), "750,580,50,20");
    assert_eq!(
        eval(&mut page, "document.elementFromPoint(760, 590).id"),
        "f"
    );
}

#[test]
fn range_rectangles_follow_their_nodes() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><p id=p style="font:16px sans-serif">one two three</p>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "var r = document.createRange(); r.selectNodeContents(document.getElementById('p')); var b = r.getBoundingClientRect(); b.width > 50 && b.height > 10 && r.getClientRects().length >= 1"
        ),
        "true"
    );
}

#[test]
fn intersection_observers_measure_against_the_viewport() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><div style="height:1000px"></div><div id=t style="height:200px"></div><div style="height:2000px"></div>
        <script>
        var seen = [];
        var io = new IntersectionObserver(function (entries) {
            entries.forEach(function (e) { seen.push([e.target.id, e.isIntersecting, Math.round(e.intersectionRatio * 100), Math.round(e.boundingClientRect.top), Math.round(e.intersectionRect.height), e.rootBounds.width].join(':')); });
        }, {threshold: [0, 0.5, 1]});
        io.observe(document.getElementById('t'));
        </script>"#,
    );
    assert_eq!(eval(&mut page, "seen.join(' ')"), "t:false:0:1000:0:800");
    eval(&mut page, "scrollTo(0, 500)");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "seen.slice(1).join(' ')"),
        "t:true:50:500:100:800"
    );
    eval(&mut page, "scrollTo(0, 900)");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "seen.slice(2).join(' ')"),
        "t:true:100:100:200:800"
    );
    eval(&mut page, "scrollTo(0, 0)");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "seen.slice(3).join(' ')"),
        "t:false:0:1000:0:800"
    );
}

#[test]
fn resize_observers_report_box_sizes() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><div id=t style="width:100px;height:40px;padding:5px;border:1px solid"></div>
        <script>
        var seen = [];
        new ResizeObserver(function (entries, observer) {
            entries.forEach(function (e) { seen.push([e.target.id, e.contentRect.x, e.contentRect.width, e.contentRect.height, e.borderBoxSize[0].inlineSize, e.contentBoxSize[0].blockSize, observer instanceof ResizeObserver].join(':')); });
        }).observe(document.getElementById('t'));
        </script>"#,
    );
    assert_eq!(eval(&mut page, "seen.join(' ')"), "t:6:100:40:112:40:true");
    eval(
        &mut page,
        "document.getElementById('t').style.width = '150px'",
    );
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "seen.slice(1).join(' ')"),
        "t:6:150:40:162:40:true"
    );
    eval(
        &mut page,
        "document.getElementById('t').style.color = 'red'",
    );
    settle(&mut page);
    assert_eq!(eval(&mut page, "seen.length"), "2");
    eval(&mut page, "document.getElementById('t').remove()");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "seen.slice(2).join(' ')"),
        "t:0:0:0:0:0:true"
    );
}
