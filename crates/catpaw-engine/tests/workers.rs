//! Dedicated workers: scripts from blob and data URLs run in a scope of
//! their own and exchange messages with the page that made them.

use catpaw_engine::{LoopLimits, PageOptions, with_html};
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
        Url::parse("https://owner.test/app/page").unwrap(),
        html.to_string(),
        options(),
        f,
    )
    .expect("the page runs");
}

#[test]
fn a_blob_worker_echoes_messages_and_knows_its_scope() {
    let html = r#"<!doctype html>
<script>
  window.log = [];
  const src = `
    self.onmessage = e => {
      postMessage({
        echo: e.data,
        name: self.name,
        href: location.href.startsWith('blob:https://owner.test/'),
        origin: location.origin,
        isSelf: self === globalThis,
        hasWindow: typeof window,
        hasDocument: typeof document,
        hasWorker: typeof Worker,
        ua: navigator.userAgent.length > 0,
        hc: navigator.hardwareConcurrency > 0,
        later: typeof setTimeout,
      });
      if (e.data === 'bye') close();
    };
    postMessage('ready');
  `;
  const url = URL.createObjectURL(new Blob([src], { type: 'text/javascript' }));
  const w = new Worker(url, { name: 'echoer' });
  w.onmessage = e => {
    log.push(JSON.stringify(e.data));
    if (e.data === 'ready') {
      w.postMessage({ n: [1, 2], s: 'x' });
      w.postMessage('bye');
    }
  };
  w.onerror = e => log.push('error: ' + e.message);
  window.w = w;
</script>"#;
    run(html, |page| {
        assert!(page.is_settled(), "{:?}", page.report());
        let log = page.eval("log.join('\\n')").unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "\"ready\"");
        assert_eq!(
            lines[1],
            "{\"echo\":{\"n\":[1,2],\"s\":\"x\"},\"name\":\"echoer\",\"href\":true,\"origin\":\"https://owner.test\",\"isSelf\":true,\"hasWindow\":\"undefined\",\"hasDocument\":\"undefined\",\"hasWorker\":\"function\",\"ua\":true,\"hc\":true,\"later\":\"function\"}"
        );
        assert!(lines[2].starts_with("{\"echo\":\"bye\""), "{}", lines[2]);
        assert_eq!(lines.len(), 3);
        // The worker closed itself: nothing runs there any more.
        assert_eq!(page.workers().len(), 0);
    });
}

#[test]
fn data_url_workers_import_scripts_and_report_errors() {
    let html = r#"<!doctype html>
<script>
  window.log = [];
  const lib = URL.createObjectURL(new Blob(['self.libValue = 42;'], { type: 'text/javascript' }));
  const src = 'importScripts(' + JSON.stringify(lib) + '); postMessage(self.libValue); throw new Error("worker boom");';
  const w = new Worker('data:text/javascript,' + encodeURIComponent(src));
  w.onmessage = e => log.push('got ' + e.data);
  w.onerror = e => { log.push('error: ' + e.message); e.preventDefault(); };
  try { new Worker('https://other.test/w.js'); } catch (e) { log.push(e.name); }
</script>"#;
    run(html, |page| {
        let log = page.eval("log.join(' | ')").unwrap();
        assert_eq!(
            log,
            "SecurityError | got 42 | error: Uncaught Error: worker boom"
        );
    });
}

#[test]
fn terminate_stops_a_worker() {
    let html = r#"<!doctype html>
<script>
  window.log = [];
  const src = 'let n = 0; setInterval(() => postMessage(++n), 100);';
  const w = new Worker(URL.createObjectURL(new Blob([src])));
  w.onmessage = e => { log.push(e.data); if (e.data === 3) w.terminate(); };
</script>"#;
    run(html, |page| {
        assert!(page.is_settled(), "{:?}", page.report());
        assert_eq!(page.eval("log.join(',')").unwrap(), "1,2,3");
        assert_eq!(page.workers().len(), 0);
    });
}
