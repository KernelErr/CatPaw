//! Shadow trees: `attachShadow()`, `ShadowRoot`, and events crossing the
//! boundary.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><style>p { color: rgb(1, 2, 3) }</style><body><div id="host">light</div>
<script>
  var log = [];
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  var host = document.getElementById('host');
  var root = host.attachShadow({ mode: 'open' });
  root.innerHTML = '<p id="inner">shadow <b>text</b></p>';
  var inner = root.firstChild;
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

fn check(page: &mut BoaPage, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        let actual = match page.eval_to_string(source) {
            Ok(text) => text,
            Err(e) => format!("THROWN {e}"),
        };
        assert_eq!(actual, *expected, "{source}");
    }
}

#[test]
fn a_shadow_tree_hangs_off_its_host() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "[root instanceof ShadowRoot, root instanceof DocumentFragment, root.mode, root.host === host, host.shadowRoot === root, root.delegatesFocus, root.clonable, root.slotAssignment].join()",
                "true,true,open,true,true,false,false,named",
            ),
            (
                "inner.id + ' ' + (root.getElementById('inner') === inner) + ' ' + root.querySelector('b').textContent",
                "inner true text",
            ),
            ("root.innerHTML", "<p id=\"inner\">shadow <b>text</b></p>"),
            // The light tree does not see it.
            (
                "[document.getElementById('inner'), host.innerHTML, host.childNodes.length, host.textContent, document.querySelector('#inner')].join('|')",
                "|light|1|light|",
            ),
            (
                "[inner.isConnected, inner.getRootNode() === root, inner.getRootNode({ composed: true }) === document, inner.parentNode === root, inner.ownerDocument === document, root.isConnected].join()",
                "true,true,true,true,true,true",
            ),
            (
                "getComputedStyle(inner).display + ' ' + getComputedStyle(inner).color",
                "block rgb(1, 2, 3)",
            ),
            (
                "host.remove(); inner.isConnected + ' ' + root.isConnected",
                "false false",
            ),
            ("document.body.appendChild(host); inner.isConnected", "true"),
            // Closed trees are kept from the outside.
            (
                "var c = document.createElement('div'), cr = c.attachShadow({ mode: 'closed' }); [c.shadowRoot, cr.mode, cr.host === c].join()",
                ",closed,true",
            ),
            (
                "attempt(function () { return host.attachShadow({ mode: 'open' }); })",
                "NotSupportedError",
            ),
            (
                "attempt(function () { return document.createElement('a').attachShadow({ mode: 'open' }); })",
                "NotSupportedError",
            ),
            (
                "attempt(function () { return host.attachShadow({}); })",
                "TypeError",
            ),
            (
                "document.createElement('x-foo').attachShadow({ mode: 'open', delegatesFocus: true }).delegatesFocus",
                "true",
            ),
            (
                "document.createElement('button', { is: 'x-button' }).attachShadow({ mode: 'open' }) instanceof ShadowRoot",
                "true",
            ),
        ],
    );
}

#[test]
fn events_are_retargeted_at_the_boundary() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "inner.addEventListener('ping', function (e) { log.push('inner ' + (e.target === inner) + ' ' + (e.currentTarget === inner) + ' ' + e.composedPath().length); });
                 root.addEventListener('ping', function (e) { log.push('root ' + (e.target === inner) + ' ' + e.eventPhase); });
                 host.addEventListener('ping', function (e) { log.push('host ' + (e.target === host) + ' ' + e.eventPhase); });
                 document.addEventListener('ping', function (e) { log.push('doc ' + (e.target === host)); });
                 window.addEventListener('ping', function (e) { log.push('window capture ' + (e.target === host)); }, true);
                 var ev = new Event('ping', { bubbles: true, composed: true });
                 inner.dispatchEvent(ev);
                 log.join(', ') + ' | ' + ev.target",
                "window capture true, inner true true 7, root true 3, host true 2, doc true | null",
            ),
            // Without `composed`, the event stays in its tree.
            (
                "log = []; inner.dispatchEvent(new Event('ping', { bubbles: true })); log.join(', ')",
                "inner true true 2, root true 3",
            ),
            // A non-bubbling composed event still reaches the host, at target.
            (
                "log = []; inner.dispatchEvent(new Event('ping', { composed: true })); log.join(', ')",
                "window capture true, inner true true 7, host true 2",
            ),
            // The host's own events are ordinary.
            ("log = []; host.dispatchEvent(new Event('ping', { bubbles: true })); log.join(', ')", "window capture true, host true 2, doc true"),
        ],
    );
}

#[test]
fn custom_elements_and_observers_work_inside() {
    let mut page = load(FIXTURE);
    check(
        &mut page,
        &[
            (
                "customElements.define('x-in', class extends HTMLElement { connectedCallback() { log.push('connected ' + this.isConnected); } disconnectedCallback() { log.push('disconnected'); } });
                 root.innerHTML = '<x-in></x-in>'; root.innerHTML = ''; log.join(', ')",
                "connected true, disconnected",
            ),
            (
                "var seen = []; new MutationObserver(function (records) { seen.push(records.length); }).observe(root, { childList: true, subtree: true });
                 root.appendChild(document.createElement('i')); root.firstChild.remove(); 0",
                "0",
            ),
        ],
    );
    let report = page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(report.stop, StopReason::Idle);
    check(&mut page, &[("seen.join()", "2")]);
}
