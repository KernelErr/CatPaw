//! User input and forms: trusted event sequences, focus, typing,
//! activation, submission and the navigations they ask for.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_dom::NodeId;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, input, scripting};
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

fn settle(page: &mut BoaPage) {
    page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
}

fn find(page: &BoaPage, id: &str) -> NodeId {
    let dom = page.page().dom.borrow();
    dom.descendants(dom.document())
        .find(|&n| dom.attr(n, "id") == Some(id))
        .unwrap_or_else(|| panic!("no #{id}"))
}

fn navigation(page: &BoaPage) -> String {
    match page.page().navigation.borrow().as_ref() {
        Some(n) => format!(
            "{} {}{}",
            n.method,
            n.url,
            n.body
                .as_ref()
                .map(|(t, b)| format!(" [{t}] {}", String::from_utf8_lossy(b)))
                .unwrap_or_default()
        ),
        None => "none".to_string(),
    }
}

const LOG: &str = "<script>var log = []; function watch(el, types) { types.forEach(function (t) { el.addEventListener(t, function (e) { log.push(t + (e.isTrusted ? '' : '!') + (e.target.id ? '@' + e.target.id : '')); }); }); }</script>";

#[test]
fn a_click_runs_the_pointer_sequence_and_moves_focus() {
    let mut page = load(&format!(
        r#"<!doctype html><body style="margin:0">{LOG}<div id=box style="height:50px"><button id=b>Go</button></div><input id=i>
        <script>watch(document.getElementById('b'), ['pointerdown','mousedown','focus','pointerup','mouseup','click','pointerover','mouseenter']);
        watch(document.getElementById('i'), ['focus','blur']);</script>"#
    ));
    let b = find(&page, "b");
    page.with_cx(|cx| input::click_element(cx, b).expect("click"));
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "pointerover@b mouseenter@b pointerdown@b mousedown@b focus@b pointerup@b mouseup@b click@b"
    );
    assert_eq!(eval(&mut page, "document.activeElement.id"), "b");
    let i = find(&page, "i");
    page.with_cx(|cx| input::click_element(cx, i).expect("click"));
    assert_eq!(
        eval(&mut page, "document.activeElement.id + ' ' + log.slice(-1)"),
        "i focus@i"
    );
    // Clicking bare canvas blurs.
    page.with_cx(|cx| input::click_at(cx, 400.0, 500.0));
    assert_eq!(
        eval(
            &mut page,
            "document.activeElement.tagName + ' ' + log.slice(-1)"
        ),
        "BODY blur@i"
    );
}

#[test]
fn occluded_and_hidden_elements_refuse_clicks() {
    let mut page = load(
        r#"<!doctype html><body style="margin:0"><button id=under style="position:absolute;left:0;top:0;width:100px;height:40px">under</button><div id=over style="position:absolute;left:0;top:0;width:200px;height:200px"></div><button id=gone style="display:none">gone</button>"#,
    );
    let under = find(&page, "under");
    let over = find(&page, "over");
    let gone = find(&page, "gone");
    let result = page.with_cx(|cx| input::click_element(cx, under));
    assert_eq!(result, Err(input::InputError::Occluded { by: over }));
    let result = page.with_cx(|cx| input::click_element(cx, gone));
    assert_eq!(result, Err(input::InputError::NotVisible));
}

#[test]
fn typing_edits_the_focused_control_with_key_and_input_events() {
    let mut page = load(&format!(
        r#"<!doctype html>{LOG}<input id=i value=""><textarea id=t></textarea>
        <script>watch(document.getElementById('i'), ['keydown','keypress','beforeinput','input','keyup','change']);</script>"#
    ));
    let i = find(&page, "i");
    page.with_cx(|cx| input::focus(cx, i).expect("focus"));
    page.with_cx(|cx| input::type_text(cx, "ab").expect("type"));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('i').value + ' | ' + log.join(' ')"
        ),
        "ab | keydown@i keypress@i beforeinput@i input@i keyup@i keydown@i keypress@i beforeinput@i input@i keyup@i"
    );
    page.with_cx(|cx| input::press(cx, "Backspace").expect("press"));
    assert_eq!(eval(&mut page, "document.getElementById('i').value"), "a");
    // Tab moves on and commits the value with `change`.
    page.with_cx(|cx| input::press(cx, "Tab").expect("press"));
    assert_eq!(
        eval(
            &mut page,
            "document.activeElement.id + ' ' + log.slice(-2).join(' ')"
        ),
        "t change@i keyup@i"
    );
    page.with_cx(|cx| input::type_text(cx, "x\ny").expect("type"));
    assert_eq!(
        eval(
            &mut page,
            "JSON.stringify(document.getElementById('t').value)"
        ),
        "\"x\\ny\""
    );
    page.with_cx(|cx| input::press(cx, "Shift+Tab").expect("press"));
    assert_eq!(eval(&mut page, "document.activeElement.id"), "i");
    // A cancelled beforeinput leaves the value alone.
    eval(
        &mut page,
        "document.getElementById('i').addEventListener('beforeinput', function (e) { e.preventDefault(); })",
    );
    page.with_cx(|cx| input::type_text(cx, "zzz").expect("type"));
    assert_eq!(eval(&mut page, "document.getElementById('i').value"), "a");
}

#[test]
fn fill_check_and_select_report_their_changes() {
    let mut page = load(&format!(
        r#"<!doctype html>{LOG}<input id=i><input id=c type=checkbox><select id=s><option value=a>A</option><option value=b>B</option></select>
        <script>watch(document.getElementById('i'), ['input','change']); watch(document.getElementById('c'), ['click','input','change']); watch(document.getElementById('s'), ['input','change']);</script>"#
    ));
    let (i, c, s) = (find(&page, "i"), find(&page, "c"), find(&page, "s"));
    page.with_cx(|cx| input::fill(cx, i, "hello").expect("fill"));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('i').value + ' ' + log.join(' ')"
        ),
        "hello input@i change@i"
    );
    page.with_cx(|cx| input::set_checked(cx, c, true).expect("check"));
    page.with_cx(|cx| input::set_checked(cx, c, true).expect("check again"));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('c').checked + ' ' + log.slice(2).join(' ')"
        ),
        "true click@c input@c change@c"
    );
    page.with_cx(|cx| input::select_option(cx, s, "b").expect("select"));
    assert_eq!(
        eval(
            &mut page,
            "var s = document.getElementById('s'); [s.value, s.selectedIndex, s.options[1].selected, s.selectedOptions.length, log.slice(5).join(' ')].join('|')"
        ),
        "b|1|true|1|input@s change@s"
    );
    assert_eq!(
        page.with_cx(|cx| input::select_option(cx, s, "zzz")),
        Err(input::InputError::NotEditable)
    );
}

#[test]
fn links_and_buttons_activate() {
    let mut page = load(&format!(
        r#"<!doctype html>{LOG}<a id=link href="../other?x=1#frag">go <span id=inner>inside</span></a>
        <details id=d><summary id=sum>more</summary>hidden</details>
        <label id=lab for=cb>label</label><input id=cb type=checkbox>
        <script>watch(document.getElementById('cb'), ['click']);</script>"#
    ));
    let inner = find(&page, "inner");
    page.with_cx(|cx| input::click_element(cx, inner).expect("click"));
    assert_eq!(navigation(&page), "GET https://example.test/other?x=1#frag");
    page.page().navigation.borrow_mut().take();
    let sum = find(&page, "sum");
    page.with_cx(|cx| input::click_element(cx, sum).expect("click"));
    assert_eq!(eval(&mut page, "document.getElementById('d').open"), "true");
    let lab = find(&page, "lab");
    page.with_cx(|cx| input::click_element(cx, lab).expect("click"));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('cb').checked + ' ' + log.join(' ')"
        ),
        "true click@cb"
    );
    // A cancelled click on a link does not navigate.
    eval(
        &mut page,
        "document.getElementById('link').addEventListener('click', function (e) { e.preventDefault(); })",
    );
    page.with_cx(|cx| input::click_element(cx, inner).expect("click"));
    assert_eq!(navigation(&page), "none");
}

#[test]
fn forms_submit_by_their_method_and_enctype() {
    let mut page = load(
        r#"<!doctype html><form id=f action="/go" method=post>
        <input name=user value=bob><input name=pass type=password value="p w"><input name=off disabled value=x>
        <input type=checkbox name=c1 value=on checked><input type=checkbox name=c2 value=on>
        <input type=radio name=r value=a><input type=radio name=r value=b checked>
        <select name=sel><option>first</option><option value=v2 selected>second</option></select>
        <textarea name=ta>line1
line2</textarea>
        <button id=go name=btn value=clicked>Go</button><button type=reset id=rst>Reset</button>
        </form>
        <form id=g action="search" method=get><input name=q value="a b"><input name=empty></form>
        <script>var submits = []; document.getElementById('f').addEventListener('submit', function (e) { submits.push(e.submitter && e.submitter.id); });
        document.getElementById('f').addEventListener('formdata', function (e) { e.formData.append('extra', 'yes'); });</script>"#,
    );
    let go = find(&page, "go");
    page.with_cx(|cx| input::click_element(cx, go).expect("click"));
    assert_eq!(
        navigation(&page),
        "POST https://example.test/go [application/x-www-form-urlencoded] user=bob&pass=p+w&c1=on&r=b&sel=v2&ta=line1%0D%0Aline2&btn=clicked&extra=yes"
    );
    assert_eq!(eval(&mut page, "submits.join(',')"), "go");
    page.page().navigation.borrow_mut().take();
    // GET puts the entries in the query.
    eval(&mut page, "document.getElementById('g').requestSubmit()");
    assert_eq!(
        navigation(&page),
        "GET https://example.test/dir/search?q=a+b&empty="
    );
    page.page().navigation.borrow_mut().take();
    // Enter in a text field submits through the default button.
    let user = find(&page, "f");
    let _ = user;
    eval(&mut page, "document.querySelector('[name=user]').focus()");
    page.with_cx(|cx| input::press(cx, "Enter").expect("press"));
    assert!(
        navigation(&page).starts_with("POST https://example.test/go"),
        "{}",
        navigation(&page)
    );
    assert_eq!(eval(&mut page, "submits.join(',')"), "go,go");
    page.page().navigation.borrow_mut().take();
    // form.submit() skips the event; reset clears user edits.
    eval(
        &mut page,
        "document.querySelector('[name=user]').value = 'edited'; document.getElementById('f').submit()",
    );
    assert!(
        navigation(&page).contains("user=edited"),
        "{}",
        navigation(&page)
    );
    assert_eq!(eval(&mut page, "submits.length"), "2");
    page.page().navigation.borrow_mut().take();
    let rst = find(&page, "rst");
    page.with_cx(|cx| input::click_element(cx, rst).expect("click"));
    assert_eq!(
        eval(&mut page, "document.querySelector('[name=user]').value"),
        "bob"
    );
    // multipart and text/plain encodings
    eval(
        &mut page,
        "document.getElementById('f').enctype = 'multipart/form-data'; document.getElementById('f').submit()",
    );
    let nav = navigation(&page);
    assert!(
        nav.contains("[multipart/form-data; boundary=")
            && nav.contains("name=\"user\"\r\n\r\nbob\r\n"),
        "{nav}"
    );
    page.page().navigation.borrow_mut().take();
    eval(
        &mut page,
        "document.getElementById('f').enctype = 'text/plain'; document.getElementById('f').submit()",
    );
    assert!(
        navigation(&page).contains("[text/plain] user=bob\r\npass=p w\r\n"),
        "{}",
        navigation(&page)
    );
}

#[test]
fn validation_and_cancellation_stop_submission() {
    let mut page = load(
        r#"<!doctype html><form id=f action="/go"><input id=req name=n required><input type=email name=e value="not-an-email"><button id=go>Go</button></form>
        <script>var invalid = []; document.getElementById('f').addEventListener('invalid', function (e) { invalid.push(e.target.name); }, true);
        var cancel = false; document.getElementById('f').addEventListener('submit', function (e) { if (cancel) e.preventDefault(); });</script>"#,
    );
    let go = find(&page, "go");
    page.with_cx(|cx| input::click_element(cx, go).expect("click"));
    assert_eq!(navigation(&page), "none");
    assert_eq!(
        eval(
            &mut page,
            "invalid.join(',') + ' ' + document.getElementById('f').checkValidity()"
        ),
        "n,e false"
    );
    eval(
        &mut page,
        "document.getElementById('req').value = 'x'; document.querySelector('[name=e]').value = 'a@b.c'; cancel = true",
    );
    page.with_cx(|cx| input::click_element(cx, go).expect("click"));
    assert_eq!(navigation(&page), "none");
    eval(&mut page, "cancel = false");
    page.with_cx(|cx| input::click_element(cx, go).expect("click"));
    assert_eq!(
        navigation(&page),
        "GET https://example.test/go?n=x&e=a%40b.c"
    );
}

#[test]
fn form_collections_and_owners() {
    let mut page = load(
        r#"<!doctype html><form id=f><input name=a><input name=g type=radio value=1><input name=g type=radio value=2 checked><select name=s><option>x</option><option>y</option></select><button>b</button></form>
        <input id=outside form=f name=o>
        <label id=l><input id=in></label>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "var f = document.getElementById('f'); [f.elements.length, f.length, f.elements[0].name, f.elements.namedItem('a').name, f.elements.g.value, f.elements.g.length, f.elements.o.id, f.elements instanceof HTMLFormControlsCollection].join('|')"
        ),
        "6|6|a|a|2|2|outside|true"
    );
    assert_eq!(
        eval(
            &mut page,
            "var s = f.elements.s; [s.options.length, s.options[1].text, s.options[1].index, s.value, s.form === f, s.options instanceof HTMLOptionsCollection, document.getElementById('outside').form === f].join('|')"
        ),
        "2|y|1|x|true|true|true"
    );
    assert_eq!(
        eval(
            &mut page,
            "s.value = 'y'; s.selectedIndex + ' ' + s.options[1].selected + ' ' + (s.options[0].selected)"
        ),
        "1 true false"
    );
    assert_eq!(
        eval(
            &mut page,
            "var l = document.getElementById('l'); l.control.id + ' ' + (l.form === null)"
        ),
        "in true"
    );
}
