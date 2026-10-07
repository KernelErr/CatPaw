//! `cargo xtask wpt`: runs testharness.js tests from web-platform-tests in
//! CatPaw pages and compares the results with expectations.
//!
//! Tests are served from the sparse checkout by an in-process file server
//! standing in for WPT's: `.any.js` and `.window.js` tests get the wrapper
//! documents WPT's server would generate, `// META:` lines are honoured,
//! and the `{{host}}`-style substitutions of `.sub.` files are filled in
//! for a single origin. Python handlers cannot run, so tests that need
//! them fail, and the expectation files say so.
//!
//! Each test runs in a child process of this binary, so a test that never
//! returns (a loop the event loop cannot interrupt) or allocates without
//! bound is killed at its deadline and costs nothing more. Its subtests
//! are reported as `PASS`, `FAIL`, `TIMEOUT` or `NOTRUN`; a test whose
//! harness itself fails reports `HARNESS-ERROR`. An expectation file
//! (`tests/wpt-expectations/<dir>.txt`) lists what is known to fail, and
//! which tests to skip (killed ones go there when the file is rewritten);
//! the run fails on failures missing from it and on passes it still lists.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits};
use catpaw_web::net::{NetHost, NetRequest, NetResponse, NetResult};
use catpaw_web::{PageConfig, PageState, scripting};
use clap::Args as ClapArgs;
use url::Url;

use crate::wpt;

#[derive(ClapArgs)]
pub struct Args {
    /// Directories (relative to the WPT root) whose tests to run, e.g.
    /// `dom/nodes`. Each has its own expectation file.
    #[arg(long, required = true)]
    include: Vec<String>,
    /// Only run tests whose path contains this text.
    #[arg(long)]
    filter: Option<String>,
    /// Rewrite the expectation files with the current failures.
    #[arg(long)]
    update_expectations: bool,
    /// Print every subtest result, not only the unexpected ones.
    #[arg(long)]
    verbose: bool,
    /// Worker threads.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Virtual time each test may use, in milliseconds.
    #[arg(long, default_value_t = 10_000)]
    budget_ms: u64,
    /// Wall-clock seconds a test may take before its process is killed.
    #[arg(long, default_value_t = 45)]
    kill_after: u64,
}

/// The hidden `wpt-one` command: one test, its outcome as JSON on stdout.
#[derive(ClapArgs)]
pub struct OneArgs {
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    test: String,
    #[arg(long)]
    budget_ms: u64,
}

pub fn run_one(args: OneArgs) -> Result<()> {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_test(&args.root, &args.test, args.budget_ms)
    }))
    .unwrap_or_else(|_| Outcome::harness_error("panic"));
    println!("{}", outcome.to_json());
    Ok(())
}

const ORIGIN: &str = "http://web-platform.test:8000";

/// What this runner serves at `/resources/testharnessreport.js`: it hands
/// the results back through a global the runner reads after the page
/// settles.
const REPORT_JS: &str = r#"
add_completion_callback(function (tests, status) {
  window.__wpt_results = JSON.stringify({
    status: status.status,
    message: status.message,
    tests: tests.map(function (t) {
      return { name: t.name, status: t.status, message: t.message };
    })
  });
});
"#;

/// A stand-in for testdriver.js: the actions it offers need a browser
/// driver; here they reject, so tests that use them fail rather than hang.
const TESTDRIVER_JS: &str = r#"
window.test_driver = new Proxy({}, { get: function (_, name) {
  if (name === 'bless') return function (_intent, action) { return Promise.resolve().then(action); };
  return function () { return Promise.reject(new Error('test_driver.' + String(name) + ' is not available')); };
} });
window.test_driver_internal = { in_automation: false };
"#;

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("xml" | "xhtml") => "application/xhtml+xml",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `{{host}}`-style substitutions, for one origin.
fn substitute(text: &str) -> String {
    text.replace("{{host}}", "web-platform.test")
        .replace("{{hosts[][]}}", "web-platform.test")
        .replace("{{hosts[][www]}}", "www.web-platform.test")
        .replace("{{domains[]}}", "web-platform.test")
        .replace("{{domains[www]}}", "www.web-platform.test")
        .replace("{{domains[www1]}}", "www1.web-platform.test")
        .replace("{{domains[www2]}}", "www2.web-platform.test")
        .replace("{{ports[http][0]}}", "8000")
        .replace("{{ports[http][1]}}", "8001")
        .replace("{{ports[https][0]}}", "8443")
        .replace("{{ports[ws][0]}}", "9000")
        .replace("{{ports[wss][0]}}", "9001")
        .replace("{{location[host]}}", "web-platform.test:8000")
        .replace("{{location[hostname]}}", "web-platform.test")
        .replace("{{location[scheme]}}", "http")
}

/// The wrapper document WPT's server generates for a `.any.js` or
/// `.window.js` test.
fn wrapper(script_url: &str, source: &str) -> String {
    let mut head = String::new();
    let mut timeout = None;
    for line in source.lines() {
        let Some(meta) = line.strip_prefix("// META:") else {
            if line.trim().is_empty() || line.starts_with("//") {
                continue;
            }
            break;
        };
        let meta = meta.trim();
        if let Some(script) = meta.strip_prefix("script=") {
            head.push_str(&format!("<script src=\"{}\"></script>\n", script.trim()));
        } else if let Some(t) = meta.strip_prefix("timeout=") {
            timeout = Some(t.trim().to_string());
        } else if let Some(title) = meta.strip_prefix("title=") {
            head.push_str(&format!("<title>{}</title>\n", title.trim()));
        }
    }
    let timeout_meta = timeout
        .map(|t| format!("<meta name=\"timeout\" content=\"{t}\">\n"))
        .unwrap_or_default();
    format!(
        "<!doctype html>\n<meta charset=utf-8>\n{timeout_meta}<script>\n\
         self.GLOBAL = {{ isWindow: function() {{ return true; }}, isWorker: function() {{ return false; }}, isShadowRealm: function() {{ return false; }} }};\n\
         </script>\n\
         <script src=\"/resources/testharness.js\"></script>\n\
         <script src=\"/resources/testharnessreport.js\"></script>\n\
         {head}<div id=log></div>\n\
         <script src=\"{script_url}\"></script>\n"
    )
}

/// Serves the checkout, with the runner's own harness glue.
struct FileNet {
    root: PathBuf,
    completed: RefCell<Vec<(u64, NetResult)>>,
    next_token: Cell<u64>,
}

impl FileNet {
    fn body_for(&self, url: &Url) -> Option<(Vec<u8>, &'static str)> {
        let path = url.path();
        match path {
            "/resources/testharnessreport.js" => {
                return Some((REPORT_JS.as_bytes().to_vec(), "text/javascript"));
            }
            "/resources/testdriver.js" | "/resources/testdriver-vendor.js" => {
                return Some((TESTDRIVER_JS.as_bytes().to_vec(), "text/javascript"));
            }
            _ => {}
        }
        let relative = path.trim_start_matches('/');
        // `X.any.html` and `X.window.html` are generated from the scripts.
        for (suffix, script) in [(".any.html", ".any.js"), (".window.html", ".window.js")] {
            if let Some(stem) = relative.strip_suffix(suffix) {
                let script_path = self.root.join(format!("{stem}{script}"));
                let source = fs::read_to_string(&script_path).ok()?;
                let script_url = format!("/{stem}{script}");
                return Some((
                    wrapper(&script_url, &source).into_bytes(),
                    "text/html; charset=utf-8",
                ));
            }
        }
        let file = self.root.join(relative);
        let bytes = fs::read(&file).ok()?;
        let bytes = if relative.contains(".sub.") {
            substitute(&String::from_utf8_lossy(&bytes)).into_bytes()
        } else {
            bytes
        };
        Some((bytes, content_type(&file)))
    }

    fn respond(&self, request: &NetRequest) -> NetResult {
        let url = &request.url;
        let mut headers = vec![("access-control-allow-origin".to_string(), "*".to_string())];
        // Headers files next to a resource, as WPT's server honours them.
        let headers_file = self
            .root
            .join(format!("{}.headers", url.path().trim_start_matches('/')));
        if let Ok(extra) = fs::read_to_string(&headers_file) {
            for line in extra.lines() {
                if let Some((name, value)) = line.split_once(':') {
                    headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
                }
            }
        }
        match self.body_for(url) {
            Some((body, content_type)) => {
                if !headers.iter().any(|(n, _)| n == "content-type") {
                    headers.push(("content-type".to_string(), content_type.to_string()));
                }
                Ok(NetResponse {
                    url: url.clone(),
                    status: 200,
                    status_text: "OK".to_string(),
                    headers,
                    body,
                    redirected: false,
                })
            }
            None => Ok(NetResponse {
                url: url.clone(),
                status: 404,
                status_text: "Not Found".to_string(),
                headers,
                body: Vec::new(),
                redirected: false,
            }),
        }
    }
}

impl NetHost for FileNet {
    fn fetch_blocking(&self, request: NetRequest) -> NetResult {
        self.respond(&request)
    }

    fn start(&self, request: NetRequest) -> u64 {
        let token = self.next_token.get() + 1;
        self.next_token.set(token);
        let result = self.respond(&request);
        self.completed.borrow_mut().push((token, result));
        token
    }

    fn poll(&self, _wait: Option<Duration>) -> Vec<(u64, NetResult)> {
        std::mem::take(&mut *self.completed.borrow_mut())
    }

    fn abort(&self, token: u64) {
        self.completed.borrow_mut().retain(|(t, _)| *t != token);
    }

    fn inflight(&self) -> usize {
        self.completed.borrow().len()
    }

    fn cookies_for(&self, _url: &Url) -> String {
        String::new()
    }

    fn set_cookie(&self, _url: &Url, _cookie: &str) {}
}

/// One test file's outcome: subtest name → status, plus the harness status.
struct Outcome {
    harness: String,
    subtests: BTreeMap<String, String>,
    elapsed: Duration,
}

impl Outcome {
    fn harness_error(message: impl Into<String>) -> Self {
        Self {
            harness: format!("HARNESS-ERROR {}", message.into()),
            subtests: BTreeMap::new(),
            elapsed: Duration::ZERO,
        }
    }

    fn to_json(&self) -> String {
        serde_json::json!({
            "harness": self.harness,
            "subtests": self.subtests,
            "elapsed_ms": self.elapsed.as_millis() as u64,
        })
        .to_string()
    }

    fn from_json(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let subtests = v["subtests"]
            .as_object()?
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect();
        Some(Self {
            harness: v["harness"].as_str()?.to_string(),
            subtests,
            elapsed: Duration::from_millis(v["elapsed_ms"].as_u64().unwrap_or(0)),
        })
    }
}

fn run_test(root: &Path, test_path: &str, budget_ms: u64) -> Outcome {
    let net = Rc::new(FileNet {
        root: root.to_path_buf(),
        completed: RefCell::new(Vec::new()),
        next_token: Cell::new(0),
    });
    let url = Url::parse(&format!("{ORIGIN}/{test_path}")).expect("test URL");
    let state = Rc::new(PageState::new(url.clone(), PageConfig::default()));
    state.set_net(net.clone());
    let Ok(mut page) = BoaPage::new(state) else {
        return Outcome::harness_error("page setup");
    };
    let html = match net.body_for(&url) {
        Some((bytes, _)) => String::from_utf8_lossy(&bytes).into_owned(),
        None => return Outcome::harness_error("missing"),
    };
    let limits = LoopLimits {
        virtual_ms: budget_ms as f64,
        wall: Duration::from_secs(30),
        ..LoopLimits::default()
    };
    let started = Instant::now();
    page.with_cx(|cx| {
        scripting::load_document(cx, &html);
        event_loop::run(cx, &limits);
    });
    let results = page
        .eval_to_string("typeof window.__wpt_results === 'string' ? window.__wpt_results : ''")
        .unwrap_or_default();
    if results.is_empty() {
        let errors = page.page().errors.borrow().clone();
        let detail = errors
            .first()
            .map(|e| e.lines().next().unwrap_or_default().to_string());
        return Outcome::harness_error(format!(
            "no results{}",
            detail.map(|d| format!(": {d}")).unwrap_or_default()
        ));
    }
    let parsed: serde_json::Value = match serde_json::from_str(&results) {
        Ok(v) => v,
        Err(e) => return Outcome::harness_error(format!("unreadable results: {e}")),
    };
    let status_name = |code: i64| match code {
        0 => "PASS",
        1 => "FAIL",
        2 => "TIMEOUT",
        3 => "NOTRUN",
        4 => "PRECONDITION_FAILED",
        _ => "UNKNOWN",
    };
    let harness = match parsed["status"].as_i64() {
        Some(0) => "OK".to_string(),
        None => "HARNESS-ERROR no status".to_string(),
        Some(code) => format!(
            "HARNESS-ERROR {}: {}",
            match code {
                1 => "ERROR",
                2 => "TIMEOUT",
                3 => "PRECONDITION_FAILED",
                _ => "UNKNOWN",
            },
            parsed["message"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
        ),
    };
    let mut subtests = BTreeMap::new();
    if let Some(tests) = parsed["tests"].as_array() {
        for t in tests {
            let name = t["name"].as_str().unwrap_or_default().to_string();
            let status = status_name(t["status"].as_i64().unwrap_or(-1));
            let message = t["message"].as_str().unwrap_or_default();
            let detail = if status == "PASS" {
                status.to_string()
            } else {
                format!("{status} {}", message.lines().next().unwrap_or_default())
            };
            subtests.insert(name, detail);
        }
    }
    Outcome {
        harness,
        subtests,
        elapsed: started.elapsed(),
    }
}

/// Runs a test in a child process, killed once `kill_after` has passed.
fn run_test_isolated(
    root: &Path,
    test_path: &str,
    budget_ms: u64,
    kill_after: Duration,
) -> Outcome {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return Outcome::harness_error(format!("no executable: {e}")),
    };
    let child = Command::new(exe)
        .arg("wpt-one")
        .arg("--root")
        .arg(root)
        .arg("--test")
        .arg(test_path)
        .arg("--budget-ms")
        .arg(budget_ms.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return Outcome::harness_error(format!("spawn: {e}")),
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    // Read on a thread: the child may write more than a pipe buffers
    // before it exits.
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader.join().unwrap_or_default();
                return match Outcome::from_json(text.trim()) {
                    Some(outcome) => outcome,
                    None => Outcome::harness_error(format!("the test process ended with {status}")),
                };
            }
            Ok(None) if started.elapsed() >= kill_after => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Outcome::harness_error(format!("killed after {} s", kill_after.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Outcome::harness_error(format!("wait: {e}")),
        }
    }
}

/// The test files under `dir`, as paths relative to the WPT root.
fn collect_tests(root: &Path, dir: &str, filter: Option<&str>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut pending = vec![root.join(dir)];
    while let Some(d) = pending.pop() {
        let entries = fs::read_dir(&d).with_context(|| format!("listing {}", d.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "resources" && name != "support" && name != "tools" {
                    pending.push(path);
                }
                continue;
            }
            let test = if name.ends_with(".any.js") {
                name.replace(".any.js", ".any.html")
            } else if name.ends_with(".window.js") {
                name.replace(".window.js", ".window.html")
            } else if name.ends_with(".html") || name.ends_with(".htm") {
                if name.ends_with("-ref.html") || name.ends_with("-manual.html") {
                    continue;
                }
                name
            } else {
                continue;
            };
            let relative = path
                .parent()
                .unwrap()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let test_path = format!("{relative}/{test}");
            if filter.is_none_or(|f| test_path.contains(f)) {
                out.push(test_path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn expectations_path(root: &Path, dir: &str) -> PathBuf {
    root.join("tests")
        .join("wpt-expectations")
        .join(format!("{}.txt", dir.replace('/', "-")))
}

/// The expected failures and the tests to skip, from one file.
struct Expectations {
    failures: BTreeSet<String>,
    skipped: BTreeSet<String>,
}

fn load_expectations(path: &Path) -> Result<Expectations> {
    let mut out = Expectations {
        failures: BTreeSet::new(),
        skipped: BTreeSet::new(),
    };
    if !path.exists() {
        return Ok(out);
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // Keys keep their trailing spaces: subtest names may end in one.
    for line in text.lines().map(|l| l.trim_end_matches('\r')) {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        match line.strip_prefix("skip ") {
            Some(test) => {
                out.skipped.insert(test.trim().to_string());
            }
            None => {
                out.failures.insert(line.to_string());
            }
        }
    }
    Ok(out)
}

/// A failing result's key in an expectation file: the test path, and
/// the subtest after a tab (`-` for the harness itself), with the
/// subtest's line breaks and tabs escaped so that a key is one line.
fn key(test: &str, subtest: &str) -> String {
    let subtest = subtest
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("{test}\t{subtest}")
}

pub fn run(args: Args) -> Result<()> {
    let root = wpt::workspace_root();
    let mut dirs: Vec<&str> = vec!["resources", "common"];
    dirs.extend(args.include.iter().map(String::as_str));
    let wpt_dir = wpt::ensure_checkout(&root, &dirs)?;

    let mut all_ok = true;
    for dir in &args.include {
        let path = expectations_path(&root, dir);
        let expected = load_expectations(&path)?;
        let mut tests = collect_tests(&wpt_dir, dir, args.filter.as_deref())?;
        let before = tests.len();
        tests.retain(|t| !expected.skipped.contains(t));
        let skipped = before - tests.len();
        let total = tests.len();
        let queue = Arc::new(Mutex::new(tests));
        let results: Arc<Mutex<Vec<(String, Outcome)>>> = Arc::new(Mutex::new(Vec::new()));
        let workers: Vec<_> = (0..args.jobs.max(1))
            .map(|_| {
                let queue = queue.clone();
                let results = results.clone();
                let wpt_dir = wpt_dir.clone();
                let budget = args.budget_ms;
                let kill_after = Duration::from_secs(args.kill_after);
                std::thread::Builder::new()
                    .spawn(move || {
                        loop {
                            let next = queue.lock().unwrap().pop();
                            let Some(test) = next else { break };
                            let outcome = run_test_isolated(&wpt_dir, &test, budget, kill_after);
                            results.lock().unwrap().push((test, outcome));
                        }
                    })
                    .expect("worker thread")
            })
            .collect();
        for w in workers {
            let _ = w.join();
        }
        let mut results = Arc::try_unwrap(results)
            .ok()
            .expect("workers done")
            .into_inner()
            .unwrap();
        results.sort_by(|a, b| a.0.cmp(&b.0));

        let mut failures: BTreeSet<String> = BTreeSet::new();
        let mut killed: BTreeSet<String> = BTreeSet::new();
        let mut details: BTreeMap<String, String> = BTreeMap::new();
        let mut passed = 0usize;
        let mut subtests = 0usize;
        for (test, outcome) in &results {
            if outcome.harness != "OK" {
                let k = key(test, "-");
                failures.insert(k.clone());
                details.insert(k, outcome.harness.clone());
                if outcome.harness.contains("killed after") {
                    killed.insert(test.clone());
                }
            }
            for (name, status) in &outcome.subtests {
                subtests += 1;
                if status == "PASS" {
                    passed += 1;
                } else {
                    let k = key(test, name);
                    failures.insert(k.clone());
                    details.insert(k, status.clone());
                }
            }
            if args.verbose {
                println!(
                    "{test}: {} ({} ms)",
                    outcome.harness,
                    outcome.elapsed.as_millis()
                );
                for (name, status) in &outcome.subtests {
                    println!("  {status}  {name}");
                }
            }
        }
        let unexpected_failures: BTreeSet<String> =
            failures.difference(&expected.failures).cloned().collect();
        let unexpected_passes: BTreeSet<String> = if args.filter.is_none() {
            expected.failures.difference(&failures).cloned().collect()
        } else {
            BTreeSet::new()
        };
        for k in &unexpected_failures {
            println!("UNEXPECTED FAIL {}: {}", k.replace('\t', " › "), details[k]);
        }
        for k in &unexpected_passes {
            println!(
                "UNEXPECTED PASS {}: remove it from {}",
                k.replace('\t', " › "),
                path.display()
            );
        }
        println!(
            "wpt {dir}: {total} tests ({skipped} skipped), {subtests} subtests, {passed} passed, {} failed ({} expected), {} unexpected failures, {} unexpected passes",
            subtests - passed,
            failures.len() - unexpected_failures.len(),
            unexpected_failures.len(),
            unexpected_passes.len()
        );

        if args.update_expectations {
            let mut out = format!(
                "# Known failures in web-platform-tests {dir}: `<test>\\t<subtest>` per line,\n\
                 # `-` for the harness itself; `skip <test>` for tests not run at all\n\
                 # (those the engine cannot finish in time). Regenerate with\n\
                 # `cargo xtask wpt --include {dir} --update-expectations`.\n"
            );
            for test in expected.skipped.iter().chain(killed.iter()) {
                out.push_str("skip ");
                out.push_str(test);
                out.push('\n');
            }
            for k in &failures {
                if !killed.iter().any(|t| k.starts_with(&format!("{t}\t"))) {
                    out.push_str(k);
                    out.push('\n');
                }
            }
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        } else if !unexpected_failures.is_empty() || !unexpected_passes.is_empty() {
            all_ok = false;
        }
    }
    if !all_ok {
        anyhow::bail!("web-platform-tests results differ from the expectations");
    }
    Ok(())
}
