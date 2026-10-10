//! Custom elements: definitions, construction, upgrades and reactions.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><body><x-early id="early" data-a="1"></x-early><div id="host"></div>
<script>
  var log = [];
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  var host = document.getElementById('host');
  class XEarly extends HTMLElement {
    static get observedAttributes() { return ['data-a', 'data-b']; }
    constructor() { super(); log.push('construct ' + this.id + ' ' + this.getAttribute('data-a') + ' ' + this.isConnected); }
    connectedCallback() { log.push('connected ' + this.id); }
    disconnectedCallback() { log.push('disconnected ' + this.id); }
    attributeChangedCallback(name, oldValue, newValue, ns) { log.push('attr ' + this.id + ' ' + name + ' ' + oldValue + ' ' + newValue + ' ' + ns); }
    adoptedCallback(from, to) { log.push('adopted ' + this.id + ' ' + (to === this.ownerDocument)); }
    hello() { return 'hi from ' + this.id; }
  }
  customElements.define('x-early', XEarly);
  log.push('defined ' + (document.getElementById('early') instanceof XEarly));
</script>
<x-early id="late"></x-early>
<script>log.push('next script ' + (document.getElementById('late') instanceof XEarly));</script>"#;

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

/// Runs `source` and returns what it logged.
fn step(page: &mut BoaPage, source: &str) -> String {
    let result = eval(page, &format!("log = []; {source}; 0"));
    assert_eq!(result, "0", "{source}");
    eval(page, "log.join(' / ')")
}

#[test]
fn elements_are_upgraded_when_their_definition_arrives() {
    let mut page = load(FIXTURE);
    assert_eq!(
        eval(&mut page, "log.join(' / ')"),
        "construct early 1 true / attr early data-a null 1 null / connected early / defined true / \
         construct late null true / connected late / next script true"
    );
    assert_eq!(
        eval(&mut page, "document.getElementById('early').hello()"),
        "hi from early"
    );
    assert_eq!(
        eval(
            &mut page,
            "customElements.get('x-early') === XEarly && !customElements.get('x-nope')"
        ),
        "true"
    );
}

#[test]
fn created_elements_are_constructed_at_once() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var made = document.createElement('x-early'); log.push(made instanceof XEarly && made.hello())"
        ),
        "construct  null false / hi from "
    );
    // Reactions run before the member that caused them returns.
    assert_eq!(
        step(
            &mut page,
            "made.setAttribute('data-a', '2'); log.push('set returned'); made.id = 'made'; made.setAttribute('data-c', 'x')"
        ),
        "attr  data-a null 2 null / set returned"
    );
    assert_eq!(
        step(
            &mut page,
            "host.appendChild(made); log.push('appended'); made.remove(); log.push('removed')"
        ),
        "connected made / appended / disconnected made / removed"
    );
    assert_eq!(
        step(
            &mut page,
            "made.setAttribute('data-a', '3'); made.removeAttribute('data-a')"
        ),
        "attr made data-a 2 3 null / attr made data-a 3 null null"
    );
    assert_eq!(
        step(
            &mut page,
            "var fresh = new XEarly(); log.push([fresh instanceof HTMLElement, fresh.localName, fresh.ownerDocument === document, fresh.isConnected, fresh.hello()].join())"
        ),
        "construct  null false / true,x-early,true,false,hi from "
    );
    for (source, expected) in [
        (
            "attempt(function () { return new HTMLElement(); })",
            "TypeError",
        ),
        (
            "attempt(function () { return HTMLElement(); })",
            "TypeError",
        ),
        ("attempt(function () { return XEarly(); })", "TypeError"),
        (
            "attempt(function () { return new HTMLDivElement(); })",
            "TypeError",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn markup_and_clones_are_upgraded_too() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "host.innerHTML = '<x-early id=\"inner\" data-b=\"b\"></x-early>'; log.push('set')"
        ),
        "construct inner null true / attr inner data-b null b null / connected inner / set"
    );
    assert_eq!(
        step(
            &mut page,
            "var copy = host.firstChild.cloneNode(true); log.push(copy instanceof XEarly)"
        ),
        "construct inner null false / attr inner data-b null b null / true"
    );
    // A document without a window never upgrades anything; what moves into
    // the page does.
    assert_eq!(
        step(
            &mut page,
            "var other = new DOMParser().parseFromString('<x-early id=\"foreign\"></x-early>', 'text/html');
             var foreign = other.getElementById('foreign'); log.push(foreign instanceof XEarly);
             host.appendChild(foreign); log.push(foreign instanceof XEarly && foreign.ownerDocument === document)"
        ),
        "false / construct foreign null true / connected foreign / true"
    );
    assert_eq!(
        step(
            &mut page,
            "other.body.appendChild(foreign); log.push('moved')"
        ),
        "disconnected foreign / adopted foreign true / moved"
    );
}

#[test]
fn definitions_are_checked() {
    let mut page = load(FIXTURE);
    for (source, expected) in [
        (
            "attempt(function () { customElements.define('nodash', class extends HTMLElement {}); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { customElements.define('X-upper', class extends HTMLElement {}); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { customElements.define('font-face', class extends HTMLElement {}); })",
            "SyntaxError",
        ),
        (
            "attempt(function () { customElements.define('x-early', class extends HTMLElement {}); })",
            "NotSupportedError",
        ),
        (
            "attempt(function () { customElements.define('x-again', XEarly); })",
            "NotSupportedError",
        ),
        (
            "attempt(function () { customElements.define('x-fn', 5); })",
            "TypeError",
        ),
        // Any constructor will do, even one that cannot construct an element.
        (
            "attempt(function () { customElements.define('x-fn', function () {}); })",
            "undefined",
        ),
        (
            "attempt(function () { customElements.define('x-cb', class extends HTMLElement { get attributeChangedCallback() { return 5; } }); })",
            "TypeError",
        ),
        (
            "attempt(function () { customElements.define('x-ext', class extends HTMLElement {}, { extends: 'x-early' }); })",
            "NotSupportedError",
        ),
        (
            "attempt(function () { customElements.define('x-ext', class extends HTMLElement {}, { extends: 'nosuch' }); })",
            "NotSupportedError",
        ),
        (
            "attempt(function () { return customElements.whenDefined('bad'); })",
            "[object Promise]",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
    // A reader that defines another element meanwhile is refused.
    assert_eq!(
        eval(
            &mut page,
            "attempt(function () {
               class Nested extends HTMLElement {
                 static get observedAttributes() { customElements.define('x-inner', class extends HTMLElement {}); return []; }
                 attributeChangedCallback() {}
               }
               customElements.define('x-nested', Nested);
             })"
        ),
        "NotSupportedError"
    );
    assert_eq!(
        eval(
            &mut page,
            "customElements.get('x-nested') + ' ' + customElements.get('x-inner')"
        ),
        "undefined undefined"
    );
}

#[test]
fn misbehaving_constructors_leave_failed_elements() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "class XBad extends HTMLElement { constructor() { super(); this.setAttribute('x', 'y'); } }
             customElements.define('x-bad', XBad);
             var bad = document.createElement('x-bad');
             log.push([bad instanceof HTMLUnknownElement, bad instanceof XBad, bad.localName, bad.hasAttribute('x')].join())"
        ),
        "true,false,x-bad,false"
    );
    assert_eq!(
        step(
            &mut page,
            "class XThrows extends HTMLElement { constructor() { super(); throw new Error('no'); } }
             customElements.define('x-throws', XThrows);
             host.innerHTML = '<x-throws id=\"t\"></x-throws>';
             var t = document.getElementById('t'); log.push(t instanceof XThrows); t.setAttribute('x', '1'); log.push(t.getAttribute('x'))"
        ),
        // super() had already given the element its prototype.
        "true / 1"
    );
    let errors = page.page().errors.borrow().clone();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].contains("NotSupportedError"), "{errors:?}");
    assert!(errors[1].contains("no"), "{errors:?}");
}

#[test]
fn promises_and_customized_built_ins() {
    let mut page = load(FIXTURE);
    assert_eq!(
        eval(
            &mut page,
            "log = [];
             customElements.whenDefined('x-early').then(function (c) { log.push('early ' + (c === XEarly)); });
             customElements.whenDefined('x-later').then(function (c) { log.push('later ' + (c === XLater)); });
             class XLater extends HTMLElement {}
             customElements.define('x-later', XLater);
             customElements.whenDefined('bad name').catch(function (e) { log.push(e.name); });
             0"
        ),
        "0"
    );
    assert_eq!(
        eval(&mut page, "log.join(' / ')"),
        "early true / later true / SyntaxError"
    );

    assert_eq!(
        step(
            &mut page,
            "class FancyButton extends HTMLButtonElement { constructor() { super(); this.fancy = true; } connectedCallback() { log.push('fancy connected ' + this.id); } }
             customElements.define('fancy-button', FancyButton, { extends: 'button' });
             var b = document.createElement('button', { is: 'fancy-button' });
             log.push([b instanceof FancyButton, b instanceof HTMLButtonElement, b.fancy, b.hasAttribute('is'), b.localName].join());
             host.innerHTML = '<button is=\"fancy-button\" id=\"fb\"></button><button id=\"plain\"></button>';
             var fb = document.getElementById('fb'), plain = document.getElementById('plain');
             log.push((fb instanceof FancyButton) + ' ' + (plain instanceof FancyButton) + ' ' + new FancyButton().localName)"
        ),
        "true,true,true,false,button / fancy connected fb / true false button"
    );
    assert_eq!(
        eval(
            &mut page,
            "class Wrong extends HTMLElement {}
             customElements.define('x-wrong', Wrong, { extends: 'button' });
             attempt(function () { return new Wrong(); })"
        ),
        "TypeError"
    );
}

/// Setting a style property or a `dataset` entry is a reaction scope of
/// its own: the attribute change reaches the element before the set
/// returns. (A component that mirrors its `style` attribute back into
/// `this.style` otherwise saw stale values and never stopped.)
#[test]
fn named_property_sets_react_at_once() {
    let mut page = load(
        r#"<script>
  var log = [];
  class XStyle extends HTMLElement {
    static get observedAttributes() { return ['style', 'data-x']; }
    attributeChangedCallback(name, oldValue, newValue) {
      log.push(name + ' ' + oldValue + ' ' + newValue);
      if (name === 'style' && oldValue !== newValue && log.length < 20) this.style = newValue;
    }
  }
  customElements.define('x-style', XStyle);
  var el = document.createElement('x-style');
</script>"#,
    );
    assert_eq!(
        step(&mut page, "el.style.color = 'red'"),
        "style null color: red; / style color: red; color: red;"
    );
    assert_eq!(step(&mut page, "el.dataset.x = '1'"), "data-x null 1");
    assert_eq!(step(&mut page, "delete el.dataset.x"), "data-x 1 null");
}
