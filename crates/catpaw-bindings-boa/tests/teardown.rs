//! A page goes away when its `BoaPage` does: nothing in the page keeps a
//! strong reference to its own state, and the roots it held are released.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const BUSY: &str = r#"<!doctype html><body><p id=p>x</p>
<script>
  var o = new MutationObserver(function () {});
  o.observe(document.body, { childList: true, subtree: true });
  setTimeout(function () { document.body.appendChild(document.createElement('div')); }, 1);
  requestAnimationFrame(function () {});
  queueMicrotask(function () {});
  new Promise(function (r) { setTimeout(r, 2); }).then(function () {});
  document.body.addEventListener('click', function () {});
  var sheet = new CSSStyleSheet(); sheet.replaceSync('p { color: red }');
  document.adoptedStyleSheets = [sheet];
  getComputedStyle(document.getElementById('p')).color;
  var c = document.createElement('x-el'); customElements.define('x-el', class extends HTMLElement {});
  document.body.appendChild(c);
  var ac = new AbortController(); fetch('/nope', { signal: ac.signal }).catch(function () {});
  document.fonts.ready.then(function () {});
</script></body></html>"#;

#[test]
fn a_dropped_page_frees_its_state() {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let weak = Rc::downgrade(&state);
    {
        let mut page = BoaPage::new(state).expect("page setup");
        page.with_cx(|cx| {
            scripting::load_document(cx, BUSY);
            event_loop::run(cx, &LoopLimits::default());
        });
        assert_eq!(
            page.eval_to_string("document.querySelectorAll('div').length")
                .unwrap(),
            "1"
        );
    }
    assert_eq!(
        weak.strong_count(),
        0,
        "the page state is still referenced after the page was dropped"
    );
}
