//! Drags: with the mouse alone, and as HTML drag and drop for draggable
//! elements, with the `DataTransfer` that carries the data.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_dom::NodeId;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::input::{self, InputError};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/dir/page.html").unwrap(),
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

fn find(page: &BoaPage, id: &str) -> NodeId {
    let dom = page.page().dom.borrow();
    dom.descendants(dom.document())
        .find(|&n| dom.attr(n, "id") == Some(id))
        .unwrap_or_else(|| panic!("no #{id}"))
}

fn drag(page: &mut BoaPage, from: &str, to: &str) -> Result<(), InputError> {
    let (from, to) = (find(page, from), find(page, to));
    page.with_cx(|cx| input::drag_element(cx, from, to))
}

/// Boxes in a row at the top of the page, 100 pixels wide with gaps of
/// 50 between them, each named by its id.
fn row(ids: &[(&str, &str)]) -> String {
    ids.iter()
        .enumerate()
        .map(|(i, (id, attrs))| {
            format!(
                r#"<div id={id} {attrs} style="position:absolute;left:{}px;top:0;width:100px;height:100px">{id}</div>"#,
                i * 150
            )
        })
        .collect()
}

#[test]
fn a_mouse_drag_crosses_elements_and_lands_on_the_target() {
    let mut page = load(&format!(
        r#"<!doctype html><body id=page style="margin:0;height:600px">{}
        <script>var log = [], moves = [];
        document.getElementById('mid').innerHTML = '<div id=inner style="height:100px"></div>';
        function watch(id, types) {{ types.forEach(function (t) {{ document.getElementById(id).addEventListener(t, function (e) {{ log.push(t + '@' + e.target.id); }}); }}); }}
        watch('from', ['mouseleave']); watch('mid', ['mouseenter', 'mouseleave', 'pointerleave']); watch('inner', ['mouseover']); watch('to', ['mouseover', 'mouseenter', 'mouseup']);
        document.addEventListener('mousemove', function (e) {{ moves.push(e.buttons + '@' + e.target.id); }});</script>"#,
        row(&[("from", ""), ("mid", ""), ("to", "")])
    ));
    drag(&mut page, "from", "to").expect("the drag lands on the target");
    // The pointer leaves the dragged box, enters the middle one (and what
    // is inside it), leaves it and enters the target, with the button held.
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "mouseleave@from mouseover@inner mouseenter@mid pointerleave@mid mouseleave@mid mouseover@to mouseenter@to mouseup@to"
    );
    assert_eq!(
        eval(&mut page, "moves.join(' ')"),
        "0@from 1@page 1@inner 1@inner 1@page 1@to"
    );
}

#[test]
fn a_drop_that_misses_the_target_fails() {
    // Something comes over the target while the button is held.
    let mut page = load(&format!(
        r#"<!doctype html><body style="margin:0">{}
        <div id=cover style="display:none;position:absolute;left:250px;top:0;width:200px;height:100px;z-index:1"></div>
        <script>var n = 0, up = '';
        document.addEventListener('mousemove', function (e) {{ if (e.buttons && ++n === 3) document.getElementById('cover').style.display = 'block'; }});
        document.addEventListener('mouseup', function (e) {{ up = e.target.id; }});</script>"#,
        row(&[("from", ""), ("mid", ""), ("to", "")])
    ));
    let cover = find(&page, "cover");
    assert_eq!(
        drag(&mut page, "from", "to"),
        Err(InputError::Occluded { by: cover })
    );
    assert_eq!(eval(&mut page, "up"), "cover");

    // The target is too far from the dragged element to show with it.
    let mut page = load(
        r#"<!doctype html><body style="margin:0;height:3000px">
        <div id=from style="position:absolute;left:0;top:0;width:100px;height:100px"></div>
        <div id=to style="position:absolute;left:0;top:2000px;width:100px;height:100px"></div>
        <script>var downs = 0; document.addEventListener('mousedown', function () { downs++; });</script>"#,
    );
    assert_eq!(drag(&mut page, "from", "to"), Err(InputError::NotVisible));
    assert_eq!(eval(&mut page, "downs"), "0");
}

/// Logs the drag events (and the mouse and pointer events that should not
/// come during a drag) by type and target.
const DRAG_LOG: &str = "<script>var log = [];
['dragstart', 'drag', 'dragenter', 'dragleave', 'dragover', 'drop', 'dragend', 'mousemove', 'mouseup', 'pointercancel'].forEach(function (t) {
  document.addEventListener(t, function (e) { log.push(t + '@' + (e.target.id || e.target.nodeName)); });
});</script>";

#[test]
fn a_draggable_element_is_dragged_and_dropped() {
    let mut page = load(&format!(
        r#"<!doctype html><body style="margin:0">{}{DRAG_LOG}
        <script>var seen = {{}}, started;
        card.addEventListener('dragstart', function (e) {{ started = e.dataTransfer; e.dataTransfer.setData('text/plain', 'card-1'); e.dataTransfer.effectAllowed = 'move'; }});
        zone.addEventListener('dragover', function (e) {{ e.preventDefault(); var t = e.dataTransfer; seen.over = [t.types.join(), JSON.stringify(t.getData('text/plain')), t.effectAllowed, t.dropEffect].join(' '); }});
        zone.addEventListener('drop', function (e) {{ e.preventDefault(); seen.drop = e.dataTransfer.getData('Text') + ' ' + e.dataTransfer.dropEffect + ' ' + (e instanceof DragEvent) + ' ' + e.isTrusted; }});
        card.addEventListener('dragend', function (e) {{ seen.end = e.dataTransfer.dropEffect; }});</script>"#,
        row(&[("card", "draggable=true"), ("other", ""), ("zone", "")])
    ));
    drag(&mut page, "card", "zone").expect("the drag lands on the zone");
    // No mouse events while the drag goes on, and none at the drop.
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "mousemove@card dragstart@card pointercancel@card \
         drag@card dragenter@HTML dragover@HTML \
         drag@card dragenter@other dragleave@HTML dragover@other \
         drag@card dragover@other \
         drag@card dragenter@HTML dragleave@other dragover@HTML \
         drag@card dragenter@zone dragleave@HTML dragover@zone \
         drop@zone dragend@card"
    );
    // The data shows only in the drop; the types throughout.
    assert_eq!(
        eval(&mut page, "[seen.over, seen.drop, seen.end].join(' | ')"),
        r#"text/plain "" move move | card-1 move true true | move"#
    );
    // The data transfer of an event that is over keeps its data to itself.
    assert_eq!(
        eval(
            &mut page,
            "started.setData('text/plain', 'later'); JSON.stringify(started.getData('text/plain')) + ' ' + started.types"
        ),
        r#""" text/plain"#
    );
}

#[test]
fn a_drop_the_target_does_not_take_fails_quietly() {
    let mut page = load(&format!(
        r#"<!doctype html><body style="margin:0">{}{DRAG_LOG}
        <script>var ended; card.addEventListener('dragend', function (e) {{ ended = e.dataTransfer.dropEffect; }});</script>"#,
        row(&[("card", "draggable=true"), ("zone", "")])
    ));
    drag(&mut page, "card", "zone").expect("released over the zone");
    assert_eq!(
        eval(&mut page, "log.slice(-3).join(' ') + ' ' + ended"),
        "dragover@zone dragleave@zone dragend@card none"
    );
}

#[test]
fn links_carry_their_url_and_a_cancelled_dragstart_leaves_the_mouse() {
    let mut page = load(&format!(
        r#"<!doctype html><body style="margin:0">{}{DRAG_LOG}
        <a id=link href="../next?x=1" style="position:absolute;left:0;top:200px;width:100px;height:40px;display:block">next</a>
        <script>var url;
        zone.addEventListener('dragover', function (e) {{ e.preventDefault(); }});
        zone.addEventListener('drop', function (e) {{ url = e.dataTransfer.getData('URL') + ' ' + e.dataTransfer.types; }});
        plain.addEventListener('dragstart', function (e) {{ e.preventDefault(); }});</script>"#,
        row(&[("plain", "draggable=true"), ("gap", ""), ("zone", "")])
    ));
    drag(&mut page, "link", "zone").expect("the link lands on the zone");
    assert_eq!(
        eval(&mut page, "url"),
        "https://example.test/next?x=1 text/uri-list"
    );
    // A drag the page cancels goes on as mouse events.
    eval(&mut page, "log = []");
    drag(&mut page, "plain", "zone").expect("the mouse drag lands on the zone");
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "mousemove@plain dragstart@plain mousemove@HTML mousemove@gap mousemove@gap mousemove@HTML mousemove@zone mouseup@zone"
    );
}

#[test]
fn script_makes_data_transfers_and_drag_events() {
    let mut page = load("<!doctype html><body>");
    assert_eq!(
        eval(
            &mut page,
            r#"var dt = new DataTransfer(); dt.setData('Text', 'a'); dt.setData('url', 'https://x.test/\r\n#c\r\nhttps://y.test/');
            [dt.types.join(), dt.getData('text/plain'), dt.getData('URL'), dt.getData('text/uri-list').split('\r\n').length, dt.dropEffect, dt.effectAllowed, dt.files.length, dt.files === dt.files].join('|')"#
        ),
        "text/plain,text/uri-list|a|https://x.test/|3|none|none|0|true"
    );
    assert_eq!(
        eval(
            &mut page,
            "dt.dropEffect = 'bogus'; var a = dt.dropEffect; dt.dropEffect = 'copy'; dt.effectAllowed = 'copyMove'; dt.setData('text', 'b'); dt.clearData('TEXT'); var b = dt.types.join(); dt.clearData(); [a, dt.dropEffect, dt.effectAllowed, b, dt.types.length].join('|')"
        ),
        "none|copy|copyMove|text/uri-list|0"
    );
    assert_eq!(
        eval(
            &mut page,
            "var ev = new DragEvent('drop', { dataTransfer: dt, clientX: 3, bubbles: true }); [ev.dataTransfer === dt, ev instanceof MouseEvent, ev.clientX, ev.bubbles, new DragEvent('x').dataTransfer].join('|')"
        ),
        "true|true|3|true|"
    );
    // An event keeps its data transfer, whatever else lets go of it.
    eval(
        &mut page,
        "var held = new DragEvent('x', { dataTransfer: new DataTransfer() }); held.dataTransfer.setData('k', 'v');",
    );
    page.with_cx(|cx| cx.script.collect_garbage());
    assert_eq!(eval(&mut page, "held.dataTransfer.getData('k')"), "v");
}
