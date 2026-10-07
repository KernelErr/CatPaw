//! Frames: iframes get pages of their own, messages cross between them,
//! and actions can address a frame.

use catpaw_engine::{FrameId, LoopLimits, PageOptions, with_html};
use url::Url;

fn options() -> PageOptions {
    PageOptions {
        limits: LoopLimits {
            wall: std::time::Duration::from_secs(10),
            virtual_ms: 5_000.0,
            max_steps: 100_000,
            settle: None,
        },
        ..PageOptions::default()
    }
}

fn run(html: &str, f: impl FnOnce(&mut catpaw_engine::Page) + Send + 'static) {
    with_html(
        Url::parse("https://parent.test/page").unwrap(),
        html.to_string(),
        options(),
        f,
    )
    .expect("the page runs");
}

#[test]
fn messages_cross_between_a_page_and_its_srcdoc_frame() {
    let html = r#"<!doctype html><title>Parent</title>
<script>
  window.log = [];
  window.addEventListener('message', e => {
    const f = document.getElementById('f');
    log.push(`${JSON.stringify(e.data)} from ${e.origin} source=${e.source === f.contentWindow}`);
  });
  window.addEventListener('load', () => log.push('parent load; frame loaded=' + frameLoaded));
  var frameLoaded = false;
</script>
<iframe id="f" srcdoc="<script>
  window.addEventListener('message', e => {
    parent.postMessage({ echo: e.data, mine: window === top, p: parent === window }, '*');
  });
  parent.postMessage('hello', '*');
</script>" onload="frameLoaded = true"></iframe>
<script>
  document.getElementById('f').addEventListener('load', () => {
    document.getElementById('f').contentWindow.postMessage({ n: [1, 2], s: 'x' }, '*');
  });
</script>"#;
    run(html, |page| {
        assert!(page.is_settled(), "{:?}", page.report());
        let log = page.eval("log.join(' | ')").unwrap();
        assert_eq!(
            log,
            "\"hello\" from https://parent.test source=true | \
             parent load; frame loaded=true | \
             {\"echo\":{\"n\":[1,2],\"s\":\"x\"},\"mine\":false,\"p\":false} from https://parent.test source=true"
        );
        let frames = page.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].parent, Some(FrameId(0)));
        assert_eq!(frames[1].url.as_str(), "about:srcdoc");
    });
}

#[test]
fn frames_nest_and_know_their_place() {
    let html = r#"<!doctype html>
<iframe id="outer" srcdoc="<iframe id='inner' srcdoc='<script>top.postMessage(parent === top ? &quot;flat&quot; : &quot;nested&quot;, &quot;*&quot;)</script>'></iframe>"></iframe>
<script>
  window.got = null;
  addEventListener('message', e => { got = e.data + ' top=' + (e.source.top === window) + ' parent=' + (e.source.parent === document.getElementById('outer').contentWindow); });
</script>"#;
    run(html, |page| {
        assert_eq!(page.eval("got").unwrap(), "nested top=true parent=true");
        let frames = page.frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].depth, 2);
        assert_eq!(frames[2].parent, Some(frames[1].id));
    });
}

#[test]
fn actions_address_the_selected_frame() {
    let html = r#"<!doctype html><title>Parent</title>
<input id="top-input">
<iframe id="f" srcdoc="<title>Child</title><input id='name'><button id='go' onclick='parent.postMessage(&quot;clicked:&quot; + document.getElementById(&quot;name&quot;).value, &quot;*&quot;)'>Go</button>"></iframe>
<script>window.got = null; addEventListener('message', e => got = e.data);</script>"#;
    run(html, |page| {
        assert!(
            page.find("#name").is_err(),
            "the child's input is not in the top document"
        );
        page.select_frame("#f").expect("the frame is open");
        assert_eq!(page.eval("document.title").unwrap(), "Child");
        page.fill("#name", "Paw").unwrap();
        page.click("#go").unwrap();
        page.select_top_frame();
        assert_eq!(page.eval("got").unwrap(), "clicked:Paw");
        assert_eq!(page.eval("document.title").unwrap(), "Parent");
        assert!(page.select_frame("#top-input").is_err());
    });
}

#[test]
fn removing_the_element_closes_the_frame() {
    let html = r#"<!doctype html>
<iframe id="f" srcdoc="<p>child</p>"></iframe>
<script>
  const f = document.getElementById('f');
  window.w = f.contentWindow;
  window.before = w.closed;
  f.remove();
</script>"#;
    run(html, |page| {
        assert_eq!(page.eval("before").unwrap(), "false");
        assert_eq!(page.eval("w.closed").unwrap(), "true");
        assert_eq!(page.frames().len(), 1);
        assert!(page.is_settled());
    });
}

#[test]
fn target_origins_are_honoured() {
    let html = r#"<!doctype html>
<iframe id="f" srcdoc="<script>
  addEventListener('message', e => parent.postMessage('got ' + e.data, '*'));
</script>"></iframe>
<script>
  window.log = [];
  addEventListener('message', e => log.push(e.data));
  document.getElementById('f').addEventListener('load', () => {
    const w = document.getElementById('f').contentWindow;
    w.postMessage('wrong', 'https://elsewhere.test');
    w.postMessage('right', 'https://parent.test');
    w.postMessage('any', '*');
    try { w.postMessage('bad', 'not a url'); } catch (e) { log.push(e.name); }
  });
</script>"#;
    run(html, |page| {
        assert_eq!(
            page.eval("log.join(',')").unwrap(),
            "SyntaxError,got right,got any"
        );
    });
}

#[test]
fn a_page_can_message_itself() {
    let html = r#"<!doctype html>
<script>
  window.got = [];
  addEventListener('message', e => got.push(e.data.x + '@' + e.origin + ':' + (e.source === window)));
  postMessage({ x: 1 }, '*');
  postMessage({ x: 2 }, 'https://parent.test');
  postMessage({ x: 3 }, 'https://other.test');
  postMessage({ x: 4 }, '/');
  window.sync = got.length;
</script>"#;
    run(html, |page| {
        assert_eq!(page.eval("sync").unwrap(), "0");
        assert_eq!(
            page.eval("got.join(' ')").unwrap(),
            "1@https://parent.test:true 2@https://parent.test:true 4@https://parent.test:true"
        );
    });
}

#[test]
fn frames_in_shadow_trees_open_too() {
    let html = r#"<!doctype html>
<script>
  window.got = null;
  addEventListener('message', e => got = e.data);
  // The iframe goes into the shadow tree before the host is connected,
  // as the Turnstile widget does it.
  const host = document.createElement('div');
  const root = host.attachShadow({ mode: 'closed' });
  const f = document.createElement('iframe');
  f.srcdoc = '<scr' + 'ipt>parent.postMessage("from shadow", "*")</scr' + 'ipt>';
  root.append(f);
  window.beforeConnect = f.contentWindow;
  document.documentElement.append(host);
  window.hasWindow = f.contentWindow !== null;
</script>"#;
    run(html, |page| {
        assert_eq!(page.eval("beforeConnect").unwrap(), "null");
        assert_eq!(page.eval("hasWindow").unwrap(), "true");
        assert_eq!(page.eval("got").unwrap(), "from shadow");
    });
}
