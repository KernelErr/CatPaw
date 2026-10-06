//! The CSS Font Loading API without any fonts: `document.fonts` as a set
//! whose faces count as loaded once asked for.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

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

/// Runs `source` (which may be asynchronous), lets the page settle, and
/// returns what was logged.
fn step(page: &mut BoaPage, source: &str) -> String {
    let result = eval(
        page,
        &format!(
            "log = []; (async function () {{ {source} }})().catch(function (e) {{ log.push('rejected ' + (e && e.name ? e.name : e)); }}); 0"
        ),
    );
    assert_eq!(result, "0", "{source}");
    settle(page);
    eval(page, "log.join(' / ')")
}

#[test]
fn the_set_and_its_faces() {
    let mut page = load("<script>var log = [];</script>");
    assert_eq!(
        eval(
            &mut page,
            "var f = document.fonts; [f === document.fonts, f.size, f.status, String(f), typeof f.addEventListener].join(' ')"
        ),
        "true 0 loaded [object FontFaceSet] function"
    );
    assert_eq!(
        eval(
            &mut page,
            "var ff = new FontFace('Roboto', 'url(/r.woff2)', { weight: '700' }); [ff.status, ff.family, ff.weight, ff.style, ff.display].join(' ')"
        ),
        "unloaded Roboto 700 normal auto"
    );
    assert_eq!(
        eval(
            &mut page,
            "f.add(ff) === f && [f.size, f.has(ff), f.add(ff) === f, f.size, [...f][0] === ff, [...f.entries()][0][1] === ff, f.check('1em Roboto')].join(' ')"
        ),
        "1 true true 1 true true true"
    );
    assert_eq!(
        step(
            &mut page,
            "var faces = await f.load('bold 1em \"Roboto\"'); log.push(faces.length, faces[0] === ff, ff.status); log.push((await ff.loaded) === ff, (await ff.load()) === ff, (await f.ready) === f, f.ready === f.ready);"
        ),
        "1 / true / loaded / true / true / true / true"
    );
    assert_eq!(
        eval(
            &mut page,
            "var seen = []; f.forEach(function (v, k, s) { seen.push(v === ff, k === ff, s === f); }); f.delete(ff) + ' ' + f.delete(ff) + ' ' + f.size + ' ' + seen.join(',')"
        ),
        "true false 0 true,true,true"
    );
    assert_eq!(
        step(
            &mut page,
            "var bin = new FontFace('Local', new ArrayBuffer(4)); log.push(bin.status, (await bin.loaded) === bin, bin.status); f.add(bin); f.clear(); log.push(f.size);"
        ),
        "loading / true / loaded / 0"
    );
}
