//! MessageChannel, MessagePort and BroadcastChannel within one page.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/page").unwrap(),
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

#[test]
fn ports_queue_until_started_and_deliver_in_order() {
    let mut page = load(
        r#"<script>
      window.log = [];
      const { port1, port2 } = new MessageChannel();
      port1.postMessage({ n: 1 });
      port1.postMessage([2]);
      log.push('posted');
      port2.addEventListener('message', e => log.push('listener ' + JSON.stringify(e.data) + ' ' + (e.source === null) + ' ' + e.origin));
      log.push('listening (not started)');
      port2.start();
      port2.onmessage = e => log.push('handler ' + JSON.stringify(e.data));
      port2.postMessage('back');
      port1.onmessage = e => log.push('port1 ' + e.data + ' ' + (e.target === port1));
      window.ports = [port1, port2];
    </script>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join(' | ')"),
        "posted | listening (not started) | listener {\"n\":1} true  | handler {\"n\":1} | listener [2] true  | handler [2] | port1 back true"
    );
    assert_eq!(
        eval(
            &mut page,
            "ports[0] instanceof MessagePort && ports[0] !== ports[1]"
        ),
        "true"
    );
    // Closing one end silences both.
    page.eval_to_string("log.length = 0; ports[0].close(); ports[1].postMessage('lost'); ports[0].postMessage('lost')").unwrap();
    page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(eval(&mut page, "log.length"), "0");
}

#[test]
fn setting_onmessage_starts_a_port() {
    let mut page = load(
        r#"<script>
      window.got = null;
      const c = new MessageChannel();
      c.port1.postMessage('early');
      c.port2.onmessage = e => got = e.data;
    </script>"#,
    );
    assert_eq!(eval(&mut page, "got"), "early");
}

#[test]
fn broadcast_channels_reach_the_others_of_their_name() {
    let mut page = load(
        r#"<script>
      window.log = [];
      const a = new BroadcastChannel('chat');
      const b = new BroadcastChannel('chat');
      const c = new BroadcastChannel('other');
      a.onmessage = e => log.push('a:' + e.data + '@' + e.origin);
      b.onmessage = e => log.push('b:' + e.data);
      c.onmessage = e => log.push('c:' + e.data);
      a.postMessage('hello');
      b.postMessage('hi');
      b.close();
      a.postMessage('again');
      try { b.postMessage('closed'); } catch (e) { log.push(e.name); }
      window.name_ = a.name;
    </script>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join(' | ')"),
        "InvalidStateError | b:hello | a:hi@https://example.test"
    );
    assert_eq!(eval(&mut page, "name_"), "chat");
}

#[test]
fn a_broadcast_listener_may_change_the_url() {
    let mut page = load(
        r#"<script>
      const a = new BroadcastChannel('nav');
      const b = new BroadcastChannel('nav');
      b.onmessage = e => { history.pushState({}, '', '/moved'); window.heard = e.data + ' ' + e.origin; };
      a.postMessage('go');
    </script>"#,
    );
    assert_eq!(
        eval(&mut page, "heard + ' ' + location.pathname"),
        "go https://example.test /moved"
    );
}
